//! Sticky-message lifecycle for the start-run UI.
//!
//! Every tier with `enable_start_run_ui = TRUE` has *two* sticky messages
//! in its configured channel:
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
use crate::db::models::{BotEmoji, DungeonTemplate, Tier};
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
    format!("srui:btn:{tier_id}")
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
         to convert it into a live raid. The slash command `/hc` does the same \
         thing if you'd rather type than click.\n\
         \n\
         \u{1F465} **Who can use it**\n\
         Anyone with the **Start Headcount** permission. Trusted operators \
         (admins / `ManageRuns`) bypass the per-user cap, post-cancel cooldown, \
         and minimum-reactor floor.\n\
         \n\
         \u{2699}\u{FE0F} **House rules**\n\
         \u{2022} One headcount per (tier, dungeon) at a time\n\
         \u{2022} One active raid per leader at a time\n\
         \u{2022} Idle headcounts auto-cancel after **{idle} minutes**\n\
         \u{2022} A short cooldown applies after you cancel your own headcount\n\
         \u{2022} A headcount needs at least **{min} reactor(s)** before it can convert\n\
         \n\
         _Live raids appear in the **Status** message below._",
        idle = tier.hc_idle_minutes,
        min = tier.hc_min_reactors,
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
    let Some(channel_id) = tier.start_run_ui_channel_id else {
        return Ok(());
    };
    let channel = serenity::ChannelId::new(channel_id as u64);

    if let Some(message_id) = tier.start_run_ui_button_message_id {
        let message = serenity::MessageId::new(message_id as u64);
        match channel.message(&serenity_ctx.http, message).await {
            Ok(_) => return Ok(()),
            Err(e) if is_not_found(&e) => {
                tracing::info!(
                    tier_id = tier.id,
                    channel_id,
                    message_id,
                    "start-run-UI button message 404; reposting",
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = ?e,
                    tier_id = tier.id,
                    channel_id,
                    "start-run-UI button probe failed; leaving as-is",
                );
                return Ok(());
            }
        }
    }

    let msg = channel
        .send_message(&serenity_ctx.http, build_button_message(pool, tier).await)
        .await?;
    db::tier::set_start_run_ui_button_message(pool, tier.id, Some(msg.id.get() as i64)).await?;
    Ok(())
}

/// Probe the configured listing message; repost it if missing.
/// Idempotent.
pub async fn ensure_listing_message(
    serenity_ctx: &serenity::Context,
    pool: &PgPool,
    tier: &Tier,
) -> Result<()> {
    let Some(channel_id) = tier.start_run_ui_channel_id else {
        return Ok(());
    };
    let channel = serenity::ChannelId::new(channel_id as u64);

    if let Some(message_id) = tier.start_run_ui_listing_message_id {
        let message = serenity::MessageId::new(message_id as u64);
        match channel.message(&serenity_ctx.http, message).await {
            Ok(_) => return Ok(()),
            Err(e) if is_not_found(&e) => {
                tracing::info!(
                    tier_id = tier.id,
                    channel_id,
                    message_id,
                    "start-run-UI listing message 404; reposting",
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = ?e,
                    tier_id = tier.id,
                    channel_id,
                    "start-run-UI listing probe failed; leaving as-is",
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
    db::tier::set_start_run_ui_listing_message(pool, tier.id, Some(msg.id.get() as i64)).await?;
    Ok(())
}

/// Best-effort delete of both sticky messages in Discord and clear
/// their stored IDs in the DB. Called when a tier is toggled out of
/// start-run UI so the channel doesn't keep a dead button around.
///
/// Failures (404, 403, network) are logged and swallowed: the DB IDs
/// are cleared regardless so a subsequent re-enable always reposts
/// fresh messages rather than chasing a stale ID.
pub async fn teardown_messages(
    serenity_ctx: &serenity::Context,
    pool: &PgPool,
    tier: &Tier,
) -> Result<()> {
    if let Some(channel_id) = tier.start_run_ui_channel_id {
        let channel = serenity::ChannelId::new(channel_id as u64);
        for (label, msg_id_opt) in [
            ("button", tier.start_run_ui_button_message_id),
            ("listing", tier.start_run_ui_listing_message_id),
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
                        "failed to delete sticky message during teardown",
                    );
                }
            }
        }
    }
    db::tier::set_start_run_ui_button_message(pool, tier.id, None).await?;
    db::tier::set_start_run_ui_listing_message(pool, tier.id, None).await?;
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
    let Some(channel_id) = tier.start_run_ui_channel_id else {
        return Ok(());
    };
    let Some(message_id) = tier.start_run_ui_listing_message_id else {
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
            db::tier::set_start_run_ui_listing_message(pool, tier.id, None).await?;
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
    let headcounts_raw = db::headcount::list_for_tier(pool, tier.id).await?;
    let runs_raw = db::run::list_for_tier(pool, tier.id).await?;
    let emoji_map = db::emoji::get_all_as_map(pool).await?;
    let stale_after = Duration::minutes(tier.hc_idle_minutes as i64);
    let now = Utc::now();

    let mut headcount_rows: Vec<ListingRow> = Vec::with_capacity(headcounts_raw.len());
    for hc in headcounts_raw {
        // Hide stale HCs from the listing: they'll get swept the next
        // time someone clicks for the same slot, but visually the
        // listing should already treat them as gone.
        if now - hc.created_at >= stale_after {
            continue;
        }
        let template = db::dungeon::get_by_id(pool, hc.dungeon_template_id).await?;
        headcount_rows.push(ListingRow {
            leader_user_id: hc.leader_user_id,
            dungeon_display: dungeon_display(template.as_ref(), hc.dungeon_template_id),
            dungeon_emoji_rendered: dungeon_emoji_rendered(template.as_ref(), &emoji_map),
            kind: ListingKind::Headcount,
            acquired_at: hc.created_at,
        });
    }

    let mut run_rows: Vec<ListingRow> = Vec::with_capacity(runs_raw.len());
    for run in runs_raw {
        let template = db::dungeon::get_by_id(pool, run.dungeon_template_id).await?;
        run_rows.push(ListingRow {
            leader_user_id: run.leader_user_id,
            dungeon_display: dungeon_display(template.as_ref(), run.dungeon_template_id),
            dungeon_emoji_rendered: dungeon_emoji_rendered(template.as_ref(), &emoji_map),
            kind: ListingKind::Run,
            acquired_at: run.created_at,
        });
    }

    let title = format!("Status \u{2022} {}", tier.name);
    let body = format_status_body(&headcount_rows, &run_rows, now);

    let footer_text = format!(
        "Click \"Start a run\" to open your own. Idle headcounts auto-cancel after {}m.",
        tier.hc_idle_minutes,
    );

    Ok(serenity::CreateEmbed::default()
        .title(title)
        .description(body)
        .footer(serenity::CreateEmbedFooter::new(footer_text))
        .color(0x57F287))
}

fn dungeon_display(template: Option<&DungeonTemplate>, fallback_id: i32) -> String {
    match template {
        Some(t) => t.display_name.clone(),
        None => format!("dungeon #{fallback_id}"),
    }
}

fn dungeon_emoji_rendered(
    template: Option<&DungeonTemplate>,
    emoji_map: &HashMap<String, BotEmoji>,
) -> String {
    template
        .and_then(|t| t.emoji.as_deref())
        .map(|name| emoji_str(name, emoji_map))
        .unwrap_or_default()
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

/// One pre-formatted listing row. Resolved from either a headcount or a
/// run; the underlying data store is collapsed to a uniform shape here.
struct ListingRow {
    leader_user_id: i64,
    dungeon_display: String,
    /// Pre-rendered Discord-emoji string (`<:name:id>`, a unicode literal,
    /// or empty if the template has no emoji or it isn't resolvable).
    dungeon_emoji_rendered: String,
    #[allow(dead_code)] // reserved for future per-kind formatting; kept for symmetry
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
