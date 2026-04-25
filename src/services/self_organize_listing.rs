//! Sticky-message lifecycle for the self-organize feature.
//!
//! Every self-organize-enabled tier has *two* sticky messages in its
//! configured channel:
//!
//! 1. A **button message** with a single "Start a run" button. This
//!    message is never edited — it's posted once and the message ID is
//!    stored on the tier. If the message is deleted in Discord (admin
//!    cleanup, channel purge), [`ensure_button_message`] reposts.
//!
//! 2. A **listing message** showing all live raids for the tier
//!    (HC + Run, leader, age). This is edited in place on every state
//!    transition (HC create / cancel / convert, run end, transfer). On
//!    404 it's reposted in the same shape as the button message.
//!
//! There is no background "chase the bottom" task. Discord pushes the
//! sticky messages up the channel as users post, but reposting on every
//! channel message would require Manage Messages (which most servers
//! won't grant a non-admin bot). The expectation is that admins lock the
//! channel down via channel permissions; the sticky messages stay
//! near the top of the visible scroll for everyone.

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use poise::serenity_prelude as serenity;
use sqlx::PgPool;

use crate::db;
use crate::db::models::{DungeonTemplate, SlotClaim, Tier};
use crate::services::channels::is_not_found;

/// Maximum rows shown in the listing embed. Discord caps embeds at 6000
/// chars total — 25 rows of "@Leader · Dungeon · HC · 12m" comfortably
/// stays under, and a tier with >25 simultaneous live raids is pathological
/// regardless. Excess raids are summarised as "+N more".
const LISTING_MAX_ROWS: usize = 25;

/// custom_id of the sticky button. Encodes the tier so a single
/// dispatcher can route clicks across all tiers.
fn button_custom_id(tier_id: i32) -> String {
    format!("so:btn:{tier_id}")
}

fn build_button_message(tier: &Tier) -> serenity::CreateMessage {
    let embed = serenity::CreateEmbed::default()
        .title(format!("Start a raid in {}", tier.name))
        .description(
            "Click below to open a headcount for any dungeon in this tier. \
             You'll pick the dungeon next, then fill in location and party.",
        )
        .color(0x5865F2);

    let button = serenity::CreateButton::new(button_custom_id(tier.id))
        .label("Start a run")
        .style(serenity::ButtonStyle::Primary)
        .emoji('\u{1F680}'); // rocket emoji — purely cosmetic

    let row = serenity::CreateActionRow::Buttons(vec![button]);
    serenity::CreateMessage::new()
        .add_embed(embed)
        .components(vec![row])
}

/// Probe the configured button message; repost it if missing.
/// Idempotent — safe to call after every config change and at boot.
pub async fn ensure_button_message(
    serenity_ctx: &serenity::Context,
    pool: &PgPool,
    tier: &Tier,
) -> Result<()> {
    let Some(channel_id) = tier.self_organize_channel_id else {
        return Ok(());
    };
    let channel = serenity::ChannelId::new(channel_id as u64);

    if let Some(message_id) = tier.self_organize_button_message_id {
        let message = serenity::MessageId::new(message_id as u64);
        match channel.message(&serenity_ctx.http, message).await {
            Ok(_) => return Ok(()),
            Err(e) if is_not_found(&e) => {
                tracing::info!(
                    tier_id = tier.id,
                    channel_id,
                    message_id,
                    "self-organize button message 404; reposting",
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = ?e,
                    tier_id = tier.id,
                    channel_id,
                    "self-organize button probe failed; leaving as-is",
                );
                return Ok(());
            }
        }
    }

    let msg = channel
        .send_message(&serenity_ctx.http, build_button_message(tier))
        .await?;
    db::tier::set_self_organize_button_message(pool, tier.id, Some(msg.id.get() as i64)).await?;
    Ok(())
}

/// Probe the configured listing message; repost it if missing.
/// Idempotent.
pub async fn ensure_listing_message(
    serenity_ctx: &serenity::Context,
    pool: &PgPool,
    tier: &Tier,
) -> Result<()> {
    let Some(channel_id) = tier.self_organize_channel_id else {
        return Ok(());
    };
    let channel = serenity::ChannelId::new(channel_id as u64);

    if let Some(message_id) = tier.self_organize_listing_message_id {
        let message = serenity::MessageId::new(message_id as u64);
        match channel.message(&serenity_ctx.http, message).await {
            Ok(_) => return Ok(()),
            Err(e) if is_not_found(&e) => {
                tracing::info!(
                    tier_id = tier.id,
                    channel_id,
                    message_id,
                    "self-organize listing message 404; reposting",
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = ?e,
                    tier_id = tier.id,
                    channel_id,
                    "self-organize listing probe failed; leaving as-is",
                );
                return Ok(());
            }
        }
    }

    let embed = render_listing_embed(serenity_ctx, pool, tier).await?;
    let msg = channel
        .send_message(
            &serenity_ctx.http,
            serenity::CreateMessage::new().add_embed(embed),
        )
        .await?;
    db::tier::set_self_organize_listing_message(pool, tier.id, Some(msg.id.get() as i64)).await?;
    Ok(())
}

/// Refresh the listing in place. Called on every HC/Run lifecycle
/// transition. If the message is missing, falls through to a repost
/// via [`ensure_listing_message`].
pub async fn refresh_listing(
    serenity_ctx: &serenity::Context,
    pool: &PgPool,
    tier: &Tier,
) -> Result<()> {
    let Some(channel_id) = tier.self_organize_channel_id else {
        return Ok(());
    };
    let Some(message_id) = tier.self_organize_listing_message_id else {
        // Never installed; nothing to refresh.
        return Ok(());
    };

    let channel = serenity::ChannelId::new(channel_id as u64);
    let message = serenity::MessageId::new(message_id as u64);
    let embed = render_listing_embed(serenity_ctx, pool, tier).await?;

    match channel
        .edit_message(
            &serenity_ctx.http,
            message,
            serenity::EditMessage::new().embed(embed),
        )
        .await
    {
        Ok(_) => Ok(()),
        Err(e) if is_not_found(&e) => {
            // Message vanished. Clear the stored ID and repost.
            tracing::info!(
                tier_id = tier.id,
                channel_id,
                message_id = message_id,
                "listing message 404; clearing and reposting",
            );
            db::tier::set_self_organize_listing_message(pool, tier.id, None).await?;
            // Re-load the tier so `ensure_listing_message` sees the
            // cleared field — a stale `tier` reference would still hold
            // the bad message_id.
            if let Some(refreshed) = db::tier::get_by_id(pool, tier.id).await? {
                ensure_listing_message(serenity_ctx, pool, &refreshed).await?;
            }
            Ok(())
        }
        Err(e) => {
            tracing::warn!(
                error = ?e,
                tier_id = tier.id,
                "failed to edit listing message",
            );
            Err(e.into())
        }
    }
}

async fn render_listing_embed(
    _serenity_ctx: &serenity::Context,
    pool: &PgPool,
    tier: &Tier,
) -> Result<serenity::CreateEmbed> {
    let claims = db::self_organize::claim_list_for_guild(pool, tier.guild_id).await?;
    let stale_after = Duration::minutes(tier.self_organize_idle_minutes as i64);
    let now = Utc::now();

    let mut rows: Vec<ListingRow> = Vec::new();
    for claim in claims.into_iter().filter(|c| c.tier_id == tier.id) {
        if let Some(row) = build_row(pool, &claim, now, stale_after).await? {
            rows.push(row);
        }
    }

    // Newest at the top so users see what's just been opened.
    rows.sort_by(|a, b| b.acquired_at.cmp(&a.acquired_at));

    let title = format!("Active raids \u{2022} {}", tier.name);

    let body = if rows.is_empty() {
        "_No active raids \u{2014} click **Start a run** above to be the first._".to_string()
    } else {
        let shown = rows.len().min(LISTING_MAX_ROWS);
        let mut s = String::with_capacity(64 * shown);
        for row in rows.iter().take(LISTING_MAX_ROWS) {
            s.push_str(&row.format(now));
            s.push('\n');
        }
        if rows.len() > LISTING_MAX_ROWS {
            s.push_str(&format!("_+{} more_", rows.len() - LISTING_MAX_ROWS));
        }
        s
    };

    let footer_text = format!(
        "Click \"Start a run\" to organize your own. \
         Idle headcounts auto-cancel after {}m.",
        tier.self_organize_idle_minutes,
    );

    Ok(serenity::CreateEmbed::default()
        .title(title)
        .description(body)
        .footer(serenity::CreateEmbedFooter::new(footer_text))
        .color(0x57F287))
}

/// One pre-formatted listing row. We resolve the dungeon template name
/// per-claim because a guild may have only a handful of live raids and
/// the per-row query keeps the rendering code straight-line.
struct ListingRow {
    leader_user_id: i64,
    dungeon_display: String,
    kind_label: &'static str,
    acquired_at: DateTime<Utc>,
}

impl ListingRow {
    fn format(&self, now: DateTime<Utc>) -> String {
        let age_min = (now - self.acquired_at).num_minutes().max(0);
        format!(
            "\u{2022} **{}** \u{00B7} <@{}> \u{00B7} `{}` \u{00B7} {}m ago",
            self.dungeon_display, self.leader_user_id, self.kind_label, age_min,
        )
    }
}

/// Build a listing row from a claim, returning `None` if the row should
/// be hidden (linked HC is older than the idle window — visually treated
/// as gone even though the actual sweep happens lazily on next click).
async fn build_row(
    pool: &PgPool,
    claim: &SlotClaim,
    now: DateTime<Utc>,
    stale_after: Duration,
) -> Result<Option<ListingRow>> {
    let template = db::dungeon::get_by_id(pool, claim.dungeon_template_id).await?;
    let dungeon_display = template
        .as_ref()
        .map(|t: &DungeonTemplate| t.display_name.clone())
        .unwrap_or_else(|| format!("dungeon #{}", claim.dungeon_template_id));

    let (kind_label, acquired_at) = if let Some(hc_id) = claim.headcount_id {
        let Some(hc) = db::headcount::get(pool, hc_id).await? else {
            // Dangling claim — orphan sweep will reconcile. Skip it.
            return Ok(None);
        };
        if now - hc.created_at >= stale_after {
            return Ok(None);
        }
        ("HC", hc.created_at)
    } else if let Some(run_id) = claim.run_id {
        let Some(run) = db::run::get(pool, run_id).await? else {
            return Ok(None);
        };
        ("Run", run.created_at)
    } else {
        // Should be unreachable per the table CHECK, but defence in depth.
        return Ok(None);
    };

    Ok(Some(ListingRow {
        leader_user_id: claim.leader_user_id,
        dungeon_display,
        kind_label,
        acquired_at,
    }))
}
