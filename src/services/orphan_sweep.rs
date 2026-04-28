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
//! 4. **Stale verification state.** Pending verification rows older than
//!    their `expires_at` are deleted (cheap GC, no Discord I/O). Each
//!    guild's persistent Verify message is fetched; on 404, the
//!    `verify_message_id` column is nulled out so the next `/setup` run
//!    knows to repost.
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
use crate::services::channels::is_not_found;
use crate::services::raid;
use crate::services::reactions::{self, reaction_types_match};
use crate::services::{self_organize_listing, voice};

/// How long a run can sit without being explicitly ended before the
/// periodic sweeper auto-ends it. Hardcoded for now — a per-tier knob
/// can land later if guilds want different policies.
pub const RUN_IDLE_HOURS: i64 = 24;

/// How long a headcount can sit before the sweeper auto-cancels it.
/// Much shorter than [`RUN_IDLE_HOURS`] because an HC that hasn't
/// converted is usually abandoned, and a self-organized HC that's still
/// alive holds a slot claim that blocks both the leader and anyone else
/// from starting the same raid. 20 minutes covers normal "gathering
/// keys" gaps without letting a forgotten HC brick the slot.
pub const HC_IDLE_MINUTES: i64 = 20;

/// How often [`spawn_idle_sweeper`] wakes up. Trades cleanup latency for
/// query rate; 5 minutes means a run/HC goes stale ≤ 5m after crossing
/// its respective idle threshold (matters more for the 20-minute HC
/// timeout than for the 24-hour run timeout).
const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

#[tracing::instrument(name = "orphan_sweep", skip_all)]
pub async fn run(ctx: &serenity::Context, pool: &PgPool) -> Result<()> {
    let mut surviving_hcs = 0usize;
    let mut deleted_hcs = 0usize;
    let mut surviving_runs = 0usize;
    let mut deleted_runs = 0usize;
    let mut deleted_vcs = 0usize;
    let mut reattached = 0usize;
    let mut cleared_verify_messages = 0usize;

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

    let expired_verifications = match db::verification::delete_expired(pool).await {
        Ok(n) => n,
        Err(e) => {
            warn!(error = ?e, "failed to delete expired verifications");
            0
        }
    };

    for (guild_id, channel_id, message_id) in db::guild::list_verify_messages(pool).await? {
        let channel = ChannelId::new(channel_id as u64);
        let message = MessageId::new(message_id as u64);
        match channel.message(&ctx.http, message).await {
            Ok(_) => {}
            Err(e) if is_not_found(&e) => {
                if let Err(e) = db::guild::set_verify_message(pool, guild_id, None).await {
                    warn!(error = ?e, guild_id, "failed to clear stale verify_message_id");
                } else {
                    cleared_verify_messages += 1;
                    info!(
                        guild_id,
                        channel_id, message_id, "cleared stale verify_message_id"
                    );
                }
            }
            Err(e) => {
                warn!(
                    error = ?e,
                    guild_id,
                    channel_id,
                    message_id,
                    "fetch verify message failed; leaving row alone",
                );
            }
        }
    }

    let dangling_claims = sweep_dangling_claims(pool).await;
    let stickies_repaired = sweep_self_organize_stickies(ctx, pool).await;

    info!(
        surviving_hcs,
        deleted_hcs,
        surviving_runs,
        deleted_runs,
        deleted_vcs,
        reattached,
        expired_verifications,
        cleared_verify_messages,
        dangling_claims,
        stickies_repaired,
        "orphan sweep complete",
    );
    Ok(())
}

/// Reconcile `self_organize_slot_claims` against the (now-swept) HC + Run
/// queues. With `ON DELETE NO ACTION` + transactional release this list
/// should always be empty, but a manual `DELETE FROM headcounts` (operator
/// surgery) or a partially-applied schema change can leak claim rows.
/// Forces them deleted so the slot doesn't stay locked forever.
async fn sweep_dangling_claims(pool: &PgPool) -> usize {
    let claims = match db::self_organize::claim_list_all(pool).await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = ?e, "failed to list self-organize claims for sweep");
            return 0;
        }
    };

    let mut dangling = 0usize;
    for claim in claims {
        let alive = match (claim.headcount_id, claim.run_id) {
            (Some(hc_id), _) => match db::headcount::get(pool, hc_id).await {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(e) => {
                    warn!(
                        error = ?e,
                        hc_id,
                        "failed to probe headcount during claim sweep; leaving claim alone",
                    );
                    continue;
                }
            },
            (None, Some(run_id)) => match db::run::get(pool, run_id).await {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(e) => {
                    warn!(
                        error = ?e,
                        run_id,
                        "failed to probe run during claim sweep; leaving claim alone",
                    );
                    continue;
                }
            },
            // CHECK constraint should make this unreachable; treat as dangling.
            (None, None) => false,
        };

        if alive {
            continue;
        }

        match db::self_organize::claim_force_delete(
            pool,
            claim.guild_id,
            claim.tier_id,
            claim.dungeon_template_id,
        )
        .await
        {
            Ok(true) => {
                info!(
                    guild_id = claim.guild_id,
                    tier_id = claim.tier_id,
                    dungeon_template_id = claim.dungeon_template_id,
                    "deleted dangling self-organize claim",
                );
                dangling += 1;
            }
            Ok(false) => {}
            Err(e) => warn!(
                error = ?e,
                guild_id = claim.guild_id,
                tier_id = claim.tier_id,
                dungeon_template_id = claim.dungeon_template_id,
                "failed to delete dangling self-organize claim",
            ),
        }
    }
    dangling
}

/// For every tier with `enable_self_organization = TRUE`, probe and repair
/// the sticky button + listing messages. The `ensure_*_message` helpers
/// are 404-aware and idempotent — a successful probe is a no-op.
async fn sweep_self_organize_stickies(ctx: &serenity::Context, pool: &PgPool) -> usize {
    let tiers = match db::tier::list_self_organize_enabled(pool).await {
        Ok(t) => t,
        Err(e) => {
            warn!(error = ?e, "failed to list self-organize tiers for sticky repair");
            return 0;
        }
    };

    let mut repaired = 0usize;
    for tier in tiers {
        if let Err(e) = self_organize_listing::ensure_button_message(ctx, pool, &tier).await {
            warn!(
                error = ?e,
                tier_id = tier.id,
                guild_id = tier.guild_id,
                "failed to repair self-organize button message",
            );
        } else {
            repaired += 1;
        }
        // Re-load so `ensure_listing_message` sees any newly-stored
        // button_message_id (and similarly for listing on its own pass).
        let refreshed = match db::tier::get_by_id(pool, tier.id).await {
            Ok(Some(t)) => t,
            Ok(None) => continue,
            Err(e) => {
                warn!(error = ?e, tier_id = tier.id, "tier vanished during sticky repair");
                continue;
            }
        };
        if let Err(e) = self_organize_listing::ensure_listing_message(ctx, pool, &refreshed).await {
            warn!(
                error = ?e,
                tier_id = tier.id,
                guild_id = tier.guild_id,
                "failed to repair self-organize listing message",
            );
        }
    }
    repaired
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

/// Find every run created more than [`RUN_IDLE_HOURS`] ago and end it.
/// Same teardown as a user-driven End click (claim release, row delete,
/// VC delete, embed edit, audit log, listing refresh) — credited to
/// `Starship (idle timeout)` rather than a user mention.
///
/// Returns the number of runs ended this pass.
async fn sweep_idle_runs(ctx: &serenity::Context, pool: &PgPool) -> usize {
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(RUN_IDLE_HOURS);
    let stale = match db::run::list_created_before(pool, cutoff).await {
        Ok(rs) => rs,
        Err(e) => {
            warn!(error = ?e, "idle-run sweep: list query failed");
            return 0;
        }
    };
    let mut ended = 0usize;
    for run in stale {
        match raid::end_run(ctx, pool, &run, None).await {
            Ok(true) => {
                info!(
                    run_id = run.id,
                    guild_id = run.guild_id,
                    age_hours = (chrono::Utc::now() - run.created_at).num_hours(),
                    "auto-ended idle run",
                );
                ended += 1;
            }
            Ok(false) => {
                // Already deleted by another path (concurrent End click,
                // a different sweeper instance). Nothing to do.
            }
            Err(e) => warn!(error = ?e, run_id = run.id, "failed to auto-end idle run"),
        }
    }
    ended
}

/// Auto-cancel headcounts older than [`HC_IDLE_HOURS`]: release any slot
/// claim, delete the row, and edit the public message to its
/// timed-out state. Best-effort on the message edit and listing refresh
/// — a stale message is annoying but the slot release is what matters.
///
/// Skips HCs with `message_id == 0`: those are the brief window between
/// `INSERT` and `set_message_id` in [`super::raid::start_headcount_inner`]
/// and belong to the boot sweep, not this one. (Sweeping them here would
/// race with a live HC creation that's just about to land its UPDATE.)
///
/// Returns the number of HCs cancelled this pass.
async fn sweep_idle_headcounts(ctx: &serenity::Context, pool: &PgPool) -> usize {
    let cutoff = chrono::Utc::now() - chrono::Duration::minutes(HC_IDLE_MINUTES);
    let stale = match db::headcount::list_created_before(pool, cutoff).await {
        Ok(hs) => hs,
        Err(e) => {
            warn!(error = ?e, "idle-hc sweep: list query failed");
            return 0;
        }
    };

    let mut cancelled = 0usize;
    let mut tiers_to_refresh: std::collections::HashSet<i32> = std::collections::HashSet::new();

    for hc in stale {
        if hc.message_id == 0 {
            continue;
        }

        let mut tx = match pool.begin().await {
            Ok(t) => t,
            Err(e) => {
                warn!(error = ?e, hc_id = hc.id, "idle-hc sweep: begin tx failed");
                continue;
            }
        };
        if let Err(e) = db::self_organize::claim_release_by_headcount(&mut tx, hc.id).await {
            warn!(error = ?e, hc_id = hc.id, "idle-hc sweep: claim release failed");
            let _ = tx.rollback().await;
            continue;
        }
        match db::headcount::delete_tx(&mut tx, hc.id).await {
            Ok(true) => {}
            Ok(false) => {
                let _ = tx.rollback().await;
                continue;
            }
            Err(e) => {
                warn!(error = ?e, hc_id = hc.id, "idle-hc sweep: delete failed");
                let _ = tx.rollback().await;
                continue;
            }
        }
        if let Err(e) = tx.commit().await {
            warn!(error = ?e, hc_id = hc.id, "idle-hc sweep: commit failed");
            continue;
        }

        cancelled += 1;
        info!(
            hc_id = hc.id,
            guild_id = hc.guild_id,
            age_minutes = (chrono::Utc::now() - hc.created_at).num_minutes(),
            "auto-cancelled idle headcount",
        );

        edit_timed_out_hc_message(ctx, pool, &hc).await;
        tiers_to_refresh.insert(hc.tier_id);
    }

    for tier_id in tiers_to_refresh {
        match db::tier::get_by_id(pool, tier_id).await {
            Ok(Some(tier)) if tier.enable_self_organization => {
                if let Err(e) = self_organize_listing::refresh_listing(ctx, pool, &tier).await {
                    warn!(
                        error = ?e,
                        tier_id,
                        "failed to refresh self-organize listing after idle HC sweep",
                    );
                }
            }
            Ok(_) => {}
            Err(e) => warn!(error = ?e, tier_id, "failed to load tier for listing refresh"),
        }
    }

    cancelled
}

/// Best-effort edit of a timed-out HC's public message to its closed
/// state. Failures (template missing, message deleted, Discord 5xx) log
/// and continue — the DB row is gone, that's the canonical state.
async fn edit_timed_out_hc_message(
    ctx: &serenity::Context,
    pool: &PgPool,
    hc: &crate::db::models::Headcount,
) {
    let template = match db::dungeon::get_by_id(pool, hc.dungeon_template_id).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            warn!(
                hc_id = hc.id,
                template_id = hc.dungeon_template_id,
                "template missing for timed-out HC; skipping message edit",
            );
            return;
        }
        Err(e) => {
            warn!(error = ?e, hc_id = hc.id, "failed to load template for timed-out HC");
            return;
        }
    };
    let emoji_map = db::emoji::get_all_as_map(pool).await.unwrap_or_default();
    let closed_embed = crate::embeds::headcount::build_closed(
        &template,
        &emoji_map,
        &format!("Headcount timed out after {HC_IDLE_MINUTES}m of inactivity."),
        true,
    );
    if let Err(e) = ChannelId::new(hc.channel_id as u64)
        .edit_message(
            &ctx.http,
            MessageId::new(hc.message_id as u64),
            serenity::EditMessage::new()
                .add_embed(closed_embed)
                .components(vec![]),
        )
        .await
    {
        warn!(
            error = ?e,
            hc_id = hc.id,
            channel_id = hc.channel_id,
            message_id = hc.message_id,
            "failed to edit timed-out HC message",
        );
    }
}

/// Spawn the periodic idle sweeper for both runs and headcounts. Runs
/// forever until the bot process exits — the task aborts cleanly when
/// the runtime drops.
///
/// Skips the immediate first tick so the boot orphan_sweep gets to
/// settle DB state before this loop starts polling.
pub fn spawn_idle_sweeper(ctx: serenity::Context, pool: PgPool) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(SWEEP_INTERVAL);
        // Default `Burst` would fire queued ticks back-to-back if a pass
        // overruns the interval. `Delay` schedules the next tick relative
        // to when the previous one finished, guaranteeing ≥SWEEP_INTERVAL
        // of quiet between passes regardless of pass duration.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // First tick fires immediately by default; skip it so we don't
        // race the boot orphan sweep.
        interval.tick().await;
        loop {
            interval.tick().await;
            let ended_runs = sweep_idle_runs(&ctx, &pool).await;
            let cancelled_hcs = sweep_idle_headcounts(&ctx, &pool).await;
            if ended_runs > 0 || cancelled_hcs > 0 {
                info!(
                    ended_runs,
                    cancelled_hcs, "idle sweep pass complete with cleanups",
                );
            }
        }
    });
}
