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

use std::collections::HashMap;

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use poise::serenity_prelude as serenity;
use sqlx::PgPool;

use crate::db;
use crate::db::models::{BotEmoji, SlotClaim, Tier};
use crate::embeds::headcount::emoji_str;
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

/// Build the sticky "Start a run" message. Async so it can render the
/// `bag_white` application emoji as a fancy bullet — falls back to a
/// unicode money-bag glyph if the emoji hasn't been uploaded yet (e.g.
/// fresh install before `sync-wiki` ran).
async fn build_button_message(pool: &PgPool, tier: &Tier) -> serenity::CreateMessage {
    let emoji_map = db::emoji::get_all_as_map(pool).await.unwrap_or_default();
    let bag = if emoji_map.contains_key("bag_white") {
        emoji_str("bag_white", &emoji_map)
    } else {
        // 💰 — picked over 🧰 because the bag-shape signals "loot run"
        // more directly than the toolbox glyph.
        "\u{1F4B0}".to_string()
    };

    let description = format!(
        "{bag} **What this does**\n\
         Open a **headcount** so other people can react to join your dungeon. \
         When enough people have signed up, click **Start Run** on the headcount \
         to convert it into a live raid.\n\
         \n\
         \u{1F465} **Who can use it**\n\
         _Anyone_ who can see this channel — no leader role required. \
         Trusted leaders (with the **Raid Leader** role, or `ManageRuns` permission) \
         bypass the per-user cap, post-cancel cooldown, and minimum-reactor floor.\n\
         \n\
         \u{2699}\u{FE0F} **House rules**\n\
         \u{2022} One raid per (tier, dungeon) at a time\n\
         \u{2022} One self-organized raid per leader at a time\n\
         \u{2022} Idle headcounts auto-cancel after **{idle} minutes**\n\
         \u{2022} A short cooldown applies after you cancel your own headcount\n\
         \u{2022} A headcount needs at least **{min} reactor(s)** before it can convert\n\
         \n\
         _Live raids appear in the **Status** message below._",
        idle = tier.self_organize_idle_minutes,
        min = tier.self_organize_min_reactors,
    );

    let embed = serenity::CreateEmbed::default()
        .title(format!("\u{1F680} Start a raid in {}", tier.name))
        .description(description)
        .color(0x5865F2);

    let button = serenity::CreateButton::new(button_custom_id(tier.id))
        .label("Start a run")
        .style(serenity::ButtonStyle::Primary)
        .emoji('\u{1F680}'); // rocket — matches the title

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
        .send_message(&serenity_ctx.http, build_button_message(pool, tier).await)
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

    let embed = render_listing_embed(pool, tier).await?;
    let msg = channel
        .send_message(
            &serenity_ctx.http,
            serenity::CreateMessage::new().add_embed(embed),
        )
        .await?;
    db::tier::set_self_organize_listing_message(pool, tier.id, Some(msg.id.get() as i64)).await?;
    Ok(())
}

/// Best-effort delete of both sticky messages in Discord and clear
/// their stored IDs in the DB. Called when a tier is toggled out of
/// self-organize so the channel doesn't keep a dead button around.
///
/// Failures (404, 403, network) are logged and swallowed: the DB IDs
/// are cleared regardless so a subsequent re-enable always reposts
/// fresh messages rather than chasing a stale ID.
pub async fn teardown_messages(
    serenity_ctx: &serenity::Context,
    pool: &PgPool,
    tier: &Tier,
) -> Result<()> {
    if let Some(channel_id) = tier.self_organize_channel_id {
        let channel = serenity::ChannelId::new(channel_id as u64);
        for (label, msg_id_opt) in [
            ("button", tier.self_organize_button_message_id),
            ("listing", tier.self_organize_listing_message_id),
        ] {
            let Some(msg_id) = msg_id_opt else { continue };
            if let Err(e) = channel
                .delete_message(&serenity_ctx.http, serenity::MessageId::new(msg_id as u64))
                .await
            {
                if !is_not_found(&e) {
                    tracing::warn!(
                        error = ?e,
                        tier_id = tier.id,
                        message_kind = label,
                        message_id = msg_id,
                        "failed to delete self-organize sticky message during teardown",
                    );
                }
            }
        }
    }
    db::tier::set_self_organize_button_message(pool, tier.id, None).await?;
    db::tier::set_self_organize_listing_message(pool, tier.id, None).await?;
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
    let embed = render_listing_embed(pool, tier).await?;

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

async fn render_listing_embed(pool: &PgPool, tier: &Tier) -> Result<serenity::CreateEmbed> {
    let claims = db::self_organize::claim_list_for_guild(pool, tier.guild_id).await?;
    let emoji_map = db::emoji::get_all_as_map(pool).await?;
    let stale_after = Duration::minutes(tier.self_organize_idle_minutes as i64);
    let now = Utc::now();

    let mut headcounts: Vec<ListingRow> = Vec::new();
    let mut runs: Vec<ListingRow> = Vec::new();
    for claim in claims.into_iter().filter(|c| c.tier_id == tier.id) {
        if let Some(row) = build_row(pool, &claim, now, stale_after, &emoji_map).await? {
            match row.kind {
                ListingKind::Headcount => headcounts.push(row),
                ListingKind::Run => runs.push(row),
            }
        }
    }

    // Newest at the top so users see what's just been opened.
    headcounts.sort_by_key(|r| std::cmp::Reverse(r.acquired_at));
    runs.sort_by_key(|r| std::cmp::Reverse(r.acquired_at));

    let title = format!("Status \u{2022} {}", tier.name);
    let body = format_status_body(&headcounts, &runs, now);

    let footer_text = format!(
        "Click \"Start a run\" to open your own. Idle headcounts auto-cancel after {}m.",
        tier.self_organize_idle_minutes,
    );

    Ok(serenity::CreateEmbed::default()
        .title(title)
        .description(body)
        .footer(serenity::CreateEmbedFooter::new(footer_text))
        .color(0x57F287))
}

/// Render the two-section body. Each section caps at `LISTING_MAX_ROWS`
/// with an overflow note so a runaway tier can't blow Discord's 4096-char
/// embed-description limit.
fn format_status_body(
    headcounts: &[ListingRow],
    runs: &[ListingRow],
    now: DateTime<Utc>,
) -> String {
    let mut body = String::with_capacity(256);

    body.push_str(&format!(
        "\u{1F4E3} **Headcounts** \u{00B7} _{}_\n",
        headcounts.len()
    ));
    if headcounts.is_empty() {
        body.push_str("_None right now \u{2014} click **Start a run** above to open one._\n");
    } else {
        for row in headcounts.iter().take(LISTING_MAX_ROWS) {
            body.push_str(&row.format(now));
            body.push('\n');
        }
        if headcounts.len() > LISTING_MAX_ROWS {
            body.push_str(&format!(
                "_+{} more_\n",
                headcounts.len() - LISTING_MAX_ROWS
            ));
        }
    }

    body.push('\n');
    body.push_str(&format!(
        "\u{2694}\u{FE0F} **Runs** \u{00B7} _{}_\n",
        runs.len()
    ));
    if runs.is_empty() {
        body.push_str("_None in progress._\n");
    } else {
        for row in runs.iter().take(LISTING_MAX_ROWS) {
            body.push_str(&row.format(now));
            body.push('\n');
        }
        if runs.len() > LISTING_MAX_ROWS {
            body.push_str(&format!("_+{} more_\n", runs.len() - LISTING_MAX_ROWS));
        }
    }

    body
}

/// Whether a listing row represents a still-collecting headcount or a
/// live run. Drives which section the row appears under.
#[derive(Clone, Copy, PartialEq)]
enum ListingKind {
    Headcount,
    Run,
}

/// One pre-formatted listing row. We resolve the dungeon template name
/// per-claim because a guild may have only a handful of live raids and
/// the per-row query keeps the rendering code straight-line.
struct ListingRow {
    leader_user_id: i64,
    dungeon_display: String,
    /// Pre-rendered Discord-emoji string (`<:name:id>`, a unicode literal,
    /// or empty if the template has no emoji or it isn't resolvable).
    dungeon_emoji_rendered: String,
    kind: ListingKind,
    acquired_at: DateTime<Utc>,
}

impl ListingRow {
    fn format(&self, now: DateTime<Utc>) -> String {
        let age_min = (now - self.acquired_at).num_minutes().max(0);
        let prefix = if self.dungeon_emoji_rendered.is_empty() {
            String::new()
        } else {
            format!("{} ", self.dungeon_emoji_rendered)
        };
        format!(
            "\u{2022} {prefix}**{}** \u{00B7} <@{}> \u{00B7} {}m ago",
            self.dungeon_display, self.leader_user_id, age_min,
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
    emoji_map: &HashMap<String, BotEmoji>,
) -> Result<Option<ListingRow>> {
    let template = db::dungeon::get_by_id(pool, claim.dungeon_template_id).await?;
    let (dungeon_display, dungeon_emoji_rendered) = match template.as_ref() {
        Some(t) => (
            t.display_name.clone(),
            t.emoji
                .as_deref()
                .map(|name| emoji_str(name, emoji_map))
                .unwrap_or_default(),
        ),
        None => (
            format!("dungeon #{}", claim.dungeon_template_id),
            String::new(),
        ),
    };

    let (kind, acquired_at) = if let Some(hc_id) = claim.headcount_id {
        let Some(hc) = db::headcount::get(pool, hc_id).await? else {
            // Dangling claim — orphan sweep will reconcile. Skip it.
            return Ok(None);
        };
        if now - hc.created_at >= stale_after {
            return Ok(None);
        }
        (ListingKind::Headcount, hc.created_at)
    } else if let Some(run_id) = claim.run_id {
        let Some(run) = db::run::get(pool, run_id).await? else {
            return Ok(None);
        };
        (ListingKind::Run, run.created_at)
    } else {
        // Should be unreachable per the table CHECK, but defence in depth.
        return Ok(None);
    };

    Ok(Some(ListingRow {
        leader_user_id: claim.leader_user_id,
        dungeon_display,
        dungeon_emoji_rendered,
        kind,
        acquired_at,
    }))
}
