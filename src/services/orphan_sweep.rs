//! Startup orphan sweep — reconciles DB rows against Discord state after a
//! (possibly crashed) restart.
//!
//! The R4 schema collapsed lifecycle to a queue: a `headcounts` / `runs`
//! row exists iff the raid is live, and terminal transitions DELETE the
//! row. That removed the original `status='active'` reconciliation pass,
//! but three residual orphan conditions remain. This module handles all
//! three on every boot.
//!
//! 1. **Stale DB rows.** A row whose Discord message was deleted while the
//!    bot was offline lives forever. Fetch each row's `(channel_id,
//!    message_id)`; if Discord returns 404, `DELETE` the row. Anything
//!    else (403 = bot kicked, 5xx = Discord blip, network error) leaves
//!    the row alone — it might come back.
//! 2. **Orphan VCs.** When a stale run row is deleted in step 1, its temp
//!    voice channel is now orphaned. Fire a best-effort delete via
//!    [`voice::delete_temp_vc`].
//! 3. **Missing reactions.** [`reactions::attach_reactions`] runs once on
//!    raid creation; a crash mid-attach can leave the message missing one
//!    or more required reactions. For each surviving headcount, diff the
//!    template's required reactions against `MessageReaction::me` and
//!    re-attach any with `me = false`.
//!
//! All steps are best-effort. Per-row failures log and continue — we'd
//! rather boot the bot with a few orphans than refuse to boot because the
//! sweep stumbled. Only a complete pool failure bubbles up.
//!
//! Called once from `main::run_bot`'s framework `setup` callback. The bot
//! is connected by then, so HTTP calls work, but interaction handlers
//! aren't dispatching yet (`BotData` doesn't exist), so there's no race
//! against live writes.

use anyhow::Result;
use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId, ReactionType};
use sqlx::PgPool;
use tracing::{info, warn};

use crate::db;
use crate::embeds::headcount::emoji_rt;
use crate::services::reactions::{self, reaction_types_match};
use crate::services::voice;

/// True iff `err` is a Discord 404. We narrowly target 404 so transient
/// failures (rate limit, 5xx, network) don't trigger destructive deletes.
fn is_not_found(err: &serenity::Error) -> bool {
    matches!(
        err,
        serenity::Error::Http(serenity::HttpError::UnsuccessfulRequest(resp))
            if resp.status_code.as_u16() == 404
    )
}

#[tracing::instrument(name = "orphan_sweep", skip_all)]
pub async fn run(ctx: &serenity::Context, pool: &PgPool) -> Result<()> {
    let mut surviving_hcs = 0usize;
    let mut deleted_hcs = 0usize;
    let mut surviving_runs = 0usize;
    let mut deleted_runs = 0usize;
    let mut deleted_vcs = 0usize;
    let mut reattached = 0usize;

    // Loaded once — every reaction reconcile needs it.
    let emoji_map = db::emoji::get_all_as_map(pool).await?;

    for hc in db::headcount::list_all(pool).await? {
        // message_id == 0 is the placeholder INSERT in `headcount::create`
        // before `set_message_id` lands. A crash in that window leaves a
        // bare row with no Discord message ever posted; sweep it.
        if hc.message_id == 0 {
            sweep_unposted_headcount(pool, hc.id, hc.channel_id, &mut deleted_hcs).await;
            continue;
        }

        let channel = ChannelId::new(hc.channel_id as u64);
        let message_id = MessageId::new(hc.message_id as u64);

        match channel.message(&ctx.http, message_id).await {
            Err(e) if is_not_found(&e) => {
                delete_stale_headcount(pool, hc.id, hc.channel_id, &mut deleted_hcs).await;
            }
            Err(e) => {
                warn!(
                    error = ?e,
                    hc_id = hc.id,
                    channel_id = hc.channel_id,
                    "fetch headcount message failed; leaving row alone",
                );
            }
            Ok(message) => {
                surviving_hcs += 1;
                reconcile_headcount_reactions(
                    ctx,
                    pool,
                    &hc,
                    &message,
                    &emoji_map,
                    &mut reattached,
                )
                .await;
            }
        }
    }

    for run in db::run::list_all(pool).await? {
        if run.message_id == 0 {
            sweep_unposted_run(
                ctx,
                pool,
                run.id,
                run.channel_id,
                run.voice_channel_id,
                &mut deleted_runs,
                &mut deleted_vcs,
            )
            .await;
            continue;
        }

        let channel = ChannelId::new(run.channel_id as u64);
        let message_id = MessageId::new(run.message_id as u64);

        match channel.message(&ctx.http, message_id).await {
            Err(e) if is_not_found(&e) => {
                delete_stale_run(
                    ctx,
                    pool,
                    run.id,
                    run.channel_id,
                    run.voice_channel_id,
                    &mut deleted_runs,
                    &mut deleted_vcs,
                )
                .await;
            }
            Err(e) => {
                warn!(
                    error = ?e,
                    run_id = run.id,
                    channel_id = run.channel_id,
                    "fetch run message failed; leaving row alone",
                );
            }
            // R4 run messages don't carry signup reactions, so no diff.
            Ok(_) => surviving_runs += 1,
        }
    }

    info!(
        surviving_hcs,
        deleted_hcs, surviving_runs, deleted_runs, deleted_vcs, reattached, "orphan sweep complete",
    );
    Ok(())
}

async fn sweep_unposted_headcount(
    pool: &PgPool,
    hc_id: i32,
    channel_id: i64,
    deleted_hcs: &mut usize,
) {
    match db::headcount::delete(pool, hc_id).await {
        Ok(true) => {
            info!(hc_id, channel_id, "deleted unposted headcount row");
            *deleted_hcs += 1;
        }
        Ok(false) => {}
        Err(e) => warn!(error = ?e, hc_id, "failed to delete unposted headcount row"),
    }
}

async fn delete_stale_headcount(
    pool: &PgPool,
    hc_id: i32,
    channel_id: i64,
    deleted_hcs: &mut usize,
) {
    match db::headcount::delete(pool, hc_id).await {
        Ok(true) => {
            info!(hc_id, channel_id, "deleted stale headcount row");
            *deleted_hcs += 1;
        }
        Ok(false) => {}
        Err(e) => warn!(error = ?e, hc_id, "failed to delete stale headcount row"),
    }
}

async fn sweep_unposted_run(
    ctx: &serenity::Context,
    pool: &PgPool,
    run_id: i32,
    channel_id: i64,
    voice_channel_id: Option<i64>,
    deleted_runs: &mut usize,
    deleted_vcs: &mut usize,
) {
    match db::run::delete(pool, run_id).await {
        Ok(true) => {
            info!(run_id, channel_id, "deleted unposted run row");
            *deleted_runs += 1;
            if let Some(vc_id) = voice_channel_id {
                voice::delete_temp_vc(&ctx.http, ChannelId::new(vc_id as u64)).await;
                *deleted_vcs += 1;
            }
        }
        Ok(false) => {}
        Err(e) => warn!(error = ?e, run_id, "failed to delete unposted run row"),
    }
}

async fn delete_stale_run(
    ctx: &serenity::Context,
    pool: &PgPool,
    run_id: i32,
    channel_id: i64,
    voice_channel_id: Option<i64>,
    deleted_runs: &mut usize,
    deleted_vcs: &mut usize,
) {
    match db::run::delete(pool, run_id).await {
        Ok(true) => {
            info!(run_id, channel_id, "deleted stale run row");
            *deleted_runs += 1;
            if let Some(vc_id) = voice_channel_id {
                voice::delete_temp_vc(&ctx.http, ChannelId::new(vc_id as u64)).await;
                *deleted_vcs += 1;
            }
        }
        Ok(false) => {}
        Err(e) => warn!(error = ?e, run_id, "failed to delete stale run row"),
    }
}

async fn reconcile_headcount_reactions(
    ctx: &serenity::Context,
    pool: &PgPool,
    hc: &crate::db::models::Headcount,
    message: &serenity::Message,
    emoji_map: &std::collections::HashMap<String, crate::db::models::BotEmoji>,
    reattached: &mut usize,
) {
    let required_list = match db::dungeon::get_reactions(pool, hc.dungeon_template_id).await {
        Ok(r) => r,
        Err(e) => {
            warn!(
                error = ?e,
                hc_id = hc.id,
                "failed to load required reactions; skipping reaction reconcile",
            );
            return;
        }
    };

    let missing: Vec<ReactionType> = required_list
        .iter()
        .filter_map(|r| emoji_rt(&r.emoji, emoji_map))
        .filter(|rt| {
            !message
                .reactions
                .iter()
                .any(|mr| mr.me && reaction_types_match(&mr.reaction_type, rt))
        })
        .collect();

    if missing.is_empty() {
        return;
    }

    let channel = ChannelId::new(hc.channel_id as u64);
    let message_id = MessageId::new(hc.message_id as u64);
    let failures = reactions::attach_reactions(&ctx.http, channel, message_id, &missing).await;
    let attached = missing.len() - failures.len();
    *reattached += attached;
    info!(
        hc_id = hc.id,
        attached,
        failed = failures.len(),
        "reattached missing headcount reactions",
    );
    if !failures.is_empty() {
        reactions::ping_organizer_on_failure(
            &ctx.http,
            channel,
            hc.leader_user_id as u64,
            message_id,
            &failures,
        )
        .await;
    }
}
