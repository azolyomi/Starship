//! Universal headcount-protection gates.
//!
//! Three responsibilities:
//!
//! 1. The **start gate** ([`check_can_start`]) enforces the slot lock,
//!    per-user cap, and post-cancel cooldown. It also lazily sweeps a
//!    stale HC for the same slot before re-checking the lock so a new
//!    user can take over an abandoned headcount in the same click.
//!
//! 2. The **convert gate** ([`check_can_convert`]) enforces the
//!    minimum-reactor floor on HC->Run conversion.
//!
//! 3. The **stale-HC sweep** ([`sweep_stale_hc_for_slot`]) — the only
//!    mechanism that releases an abandoned slot. There is no background
//!    task; the sweep is invoked lazily from the start gate and from the
//!    listing renderer (which hides stale rows so they don't visually
//!    pin the slot).
//!
//! All three apply to every headcount in every tier. The `is_organizer`
//! flag (admin / `ManageRuns` / superadmin) bypasses the cap, the
//! cooldown, and the min-reactor floor — but never the slot lock or the
//! stale sweep, which are structural invariants.

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use poise::serenity_prelude as serenity;
use sqlx::PgPool;

use crate::db::models::{DungeonTemplate, Headcount, Tier};
use crate::{db, embeds};

/// Reasons a headcount start can be refused. Each variant carries the
/// data needed to render a friendly user-facing message; the conversion
/// to `String` lives in [`RaidStartBlock::user_message`].
#[derive(Debug)]
pub enum RaidStartBlock {
    /// Another HC is already up for this slot. Carries the existing HC
    /// so we can name the holder.
    SlotInUse(Headcount),
    /// The caller is already leading another active raid (HC or run)
    /// somewhere in the guild. Bypassed for organizers.
    UserAlreadyHasRaid,
    /// The caller cancelled their own HC recently and is in cooldown
    /// for this tier. Bypassed for organizers.
    OnCooldown { until: DateTime<Utc> },
}

impl RaidStartBlock {
    /// User-facing message for an ephemeral interaction reply.
    pub fn user_message(&self) -> String {
        match self {
            RaidStartBlock::SlotInUse(holder) => format!(
                "Another headcount for this dungeon is already up (led by <@{}>). \
                 Wait for it to start or be cancelled, then try again.",
                holder.leader_user_id,
            ),
            RaidStartBlock::UserAlreadyHasRaid => {
                "You're already leading a raid. End or transfer that one before \
                 starting another."
                    .to_string()
            }
            RaidStartBlock::OnCooldown { until } => {
                // Discord renders <t:UNIX:R> as a live relative timestamp
                // ("in 4 minutes"), so users see an accurate countdown
                // without us computing one.
                format!(
                    "You cancelled your last headcount in this tier recently. \
                     Try again <t:{}:R>.",
                    until.timestamp(),
                )
            }
        }
    }
}

/// Run the start gate. Returns `Ok(None)` on pass; `Ok(Some(block))` on
/// the first failed check.
///
/// Order matters: the stale-slot sweep runs *before* the slot check, so
/// a new user can take over an abandoned slot in the same click. The
/// per-user cap and cooldown checks run last because they're cheap and
/// rarely fire — we'd rather tell the user "another raid is up" than
/// "you've been bad" if both apply.
pub async fn check_can_start(
    serenity_ctx: &serenity::Context,
    pool: &PgPool,
    tier: &Tier,
    template: &DungeonTemplate,
    caller_id: i64,
    is_organizer: bool,
) -> Result<Option<RaidStartBlock>> {
    // Stale-slot sweep: if the slot is held by an HC older than the
    // tier's idle-minutes window, kick it out before re-checking.
    if let Some(hc) = db::headcount::slot_holder(pool, tier.guild_id, tier.id, template.id).await? {
        let stale_after = Duration::minutes(tier.hc_idle_minutes as i64);
        if Utc::now() - hc.created_at >= stale_after {
            sweep_stale_hc_for_slot(serenity_ctx, pool, &hc, template).await?;
        }
    }

    // Re-load: the sweep may have released the slot.
    if let Some(hc) = db::headcount::slot_holder(pool, tier.guild_id, tier.id, template.id).await? {
        return Ok(Some(RaidStartBlock::SlotInUse(hc)));
    }

    if !is_organizer
        && db::headcount::count_active_raids_for_user(pool, tier.guild_id, caller_id).await? > 0
    {
        return Ok(Some(RaidStartBlock::UserAlreadyHasRaid));
    }

    if !is_organizer {
        if let Some(until) =
            db::raid_gates::cooldown_active(pool, tier.guild_id, tier.id, caller_id).await?
        {
            return Ok(Some(RaidStartBlock::OnCooldown { until }));
        }
    }

    Ok(None)
}

/// Reasons HC->Run conversion can be refused. Distinct from
/// [`RaidStartBlock`] because the convert path has a different lifecycle
/// (the HC already exists; we're gating the transition out of it).
#[derive(Debug)]
pub enum RaidConvertBlock {
    /// Not enough distinct reactors signed up.
    MinReactorsNotMet { observed: i64, required: i64 },
}

impl RaidConvertBlock {
    pub fn user_message(&self) -> String {
        match self {
            RaidConvertBlock::MinReactorsNotMet { observed, required } => format!(
                "This raid needs at least **{required}** reactor{plural} to start \
                 (currently **{observed}**). Wait for more signups.",
                plural = if *required == 1 { "" } else { "s" },
            ),
        }
    }
}

/// Gate the HC->Run conversion.
///
/// Counts distinct *non-bot* reactors across all reactions on the HC
/// message and rejects if fewer than `tier.hc_min_reactors` signed up.
/// The non-bot filter matters: the bot's own reactions on the message
/// (added by [`crate::services::reactions::attach_reactions`]) would
/// otherwise count for one signup each.
///
/// `is_organizer` bypass: trusted operators can convert with fewer
/// reactors than the configured floor — the floor is an anti-troll
/// guardrail, not an organizer-side rule.
pub fn check_can_convert(
    tier: &Tier,
    distinct_reactor_count: i64,
    is_organizer: bool,
) -> Option<RaidConvertBlock> {
    if is_organizer {
        return None;
    }
    let required = tier.hc_min_reactors as i64;
    if distinct_reactor_count < required {
        return Some(RaidConvertBlock::MinReactorsNotMet {
            observed: distinct_reactor_count,
            required,
        });
    }
    None
}

/// Set the post-cancel cooldown for a leader who cancelled their own
/// headcount. The duration comes from the tier's
/// `hc_cancel_cooldown_seconds` knob.
pub async fn record_post_cancel_cooldown(pool: &PgPool, tier: &Tier, user_id: i64) -> Result<()> {
    let duration = Duration::seconds(tier.hc_cancel_cooldown_seconds as i64);
    db::raid_gates::cooldown_set(pool, tier.guild_id, tier.id, user_id, duration).await?;
    Ok(())
}

/// Displace a stale HC: best-effort edit the HC message to a closed
/// embed announcing the auto-cancel, then delete the HC row. The unique
/// index on `headcounts(guild_id, tier_id, dungeon_template_id)` is
/// what was holding the slot, so the row delete frees it.
///
/// Discord-side errors (404 on the message, network failure) are
/// logged but don't fail the sweep — the row delete happens regardless,
/// so the slot is freed even if the visual cleanup fails.
pub async fn sweep_stale_hc_for_slot(
    serenity_ctx: &serenity::Context,
    pool: &PgPool,
    hc: &Headcount,
    template: &DungeonTemplate,
) -> Result<()> {
    tracing::info!(
        hc_id = hc.id,
        guild_id = hc.guild_id,
        tier_id = hc.tier_id,
        dungeon_template_id = hc.dungeon_template_id,
        "sweeping stale headcount",
    );

    // Best-effort: rewrite the abandoned message so the channel doesn't
    // show a stuck "active" embed with non-functional buttons.
    if hc.message_id != 0 {
        match db::emoji::get_all_as_map(pool).await {
            Ok(emoji_map) => {
                let closed = embeds::headcount::build_closed(
                    template,
                    &emoji_map,
                    "Headcount auto-cancelled (idle).",
                    true,
                );
                let channel = serenity::ChannelId::new(hc.channel_id as u64);
                let message_id = serenity::MessageId::new(hc.message_id as u64);
                if let Err(e) = channel
                    .edit_message(
                        &serenity_ctx.http,
                        message_id,
                        serenity::EditMessage::new()
                            .embed(closed)
                            .components(vec![]),
                    )
                    .await
                {
                    tracing::warn!(
                        hc_id = hc.id,
                        error = ?e,
                        "failed to edit stale HC message; row delete proceeding",
                    );
                }
            }
            Err(e) => {
                tracing::warn!(error = ?e, "emoji map load failed during stale sweep");
            }
        }
    }

    db::headcount::delete(pool, hc.id).await?;
    Ok(())
}
