//! Service layer for the self-organized raid feature.
//!
//! Two responsibilities:
//!
//! 1. The **anti-troll gate** ([`check_can_start`]) that runs before every
//!    self-organize click and decides whether the user is allowed to open
//!    a headcount for a given (tier, dungeon).
//!
//! 2. The **stale-HC sweep** ([`sweep_stale_hc_for_slot`]) that displaces
//!    an idle headcount when a new user attempts the same slot. This is
//!    the *only* mechanism that releases an abandoned slot — there is no
//!    background task. The sweep is invoked lazily from the gate, and the
//!    listing renderer hides stale rows so they never visually pin the
//!    slot for users who can see the listing but haven't clicked yet.
//!
//! Min-reactor enforcement at HC->Run conversion time lives in
//! [`check_can_convert`], which is called from the headcount-confirm
//! handler with the live reaction set already in hand.

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use poise::serenity_prelude as serenity;
use sqlx::PgPool;

use crate::db::models::{DungeonTemplate, SlotClaim, Tier};
use crate::{db, embeds};

/// Reasons a self-organize start can be refused. Each variant carries the
/// data needed to render a friendly user-facing message; the conversion
/// to `String` lives in [`SelfOrganizeBlock::user_message`].
#[derive(Debug)]
pub enum SelfOrganizeBlock {
    /// The tier doesn't have self-organize enabled. Defence in depth —
    /// reaching this from the sticky button means the button outlived a
    /// config change.
    TierDisabled,
    /// Another HC or run is already up for this slot. Holds the existing
    /// claim so we can name the holder.
    SlotInUse(SlotClaim),
    /// The caller is already leading another self-organized HC or run
    /// somewhere in the guild.
    UserAlreadyHasRaid,
    /// The caller cancelled their own HC recently and is in cooldown for
    /// this tier.
    OnCooldown { until: DateTime<Utc> },
}

impl SelfOrganizeBlock {
    /// User-facing message for an ephemeral interaction reply.
    pub fn user_message(&self) -> String {
        match self {
            SelfOrganizeBlock::TierDisabled => {
                "Self-organized raids are disabled for this tier.".to_string()
            }
            SelfOrganizeBlock::SlotInUse(holder) => format!(
                "Another raid for this dungeon is already up (led by <@{}>). \
                 Wait for it to end, then try again.",
                holder.leader_user_id,
            ),
            SelfOrganizeBlock::UserAlreadyHasRaid => {
                "You're already leading a raid. End or transfer that one before \
                 starting another."
                    .to_string()
            }
            SelfOrganizeBlock::OnCooldown { until } => {
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

/// Run the anti-troll gate for a self-organize start. Returns
/// `Ok(None)` on pass; `Ok(Some(block))` on the first failed check.
///
/// Order matters: the stale-slot sweep runs *before* the slot check, so a
/// new user can take over an abandoned slot in the same click. The
/// per-user cap and cooldown checks run last because they're cheap and
/// rarely fire — we'd rather tell the user "another raid is up" than
/// "you've been bad" if both apply.
pub async fn check_can_start(
    serenity_ctx: &serenity::Context,
    pool: &PgPool,
    tier: &Tier,
    template: &DungeonTemplate,
    caller_id: i64,
) -> Result<Option<SelfOrganizeBlock>> {
    if !tier.enable_self_organization {
        return Ok(Some(SelfOrganizeBlock::TierDisabled));
    }

    // Stale-slot sweep: if the slot is held by an HC older than the
    // tier's idle-minutes window, kick it out before re-checking.
    if let Some(claim) =
        db::self_organize::claim_get_by_slot(pool, tier.guild_id, tier.id, template.id).await?
    {
        if let Some(hc_id) = claim.headcount_id {
            let hc = db::headcount::get(pool, hc_id).await?;
            if let Some(hc) = hc {
                let stale_after = Duration::minutes(tier.self_organize_idle_minutes as i64);
                if Utc::now() - hc.created_at >= stale_after {
                    sweep_stale_hc_for_slot(serenity_ctx, pool, &claim, &hc, template).await?;
                }
            }
        }
    }

    // Re-load: the sweep may have released the slot.
    if let Some(claim) =
        db::self_organize::claim_get_by_slot(pool, tier.guild_id, tier.id, template.id).await?
    {
        return Ok(Some(SelfOrganizeBlock::SlotInUse(claim)));
    }

    if db::self_organize::claim_count_for_user(pool, tier.guild_id, caller_id).await? > 0 {
        return Ok(Some(SelfOrganizeBlock::UserAlreadyHasRaid));
    }

    if let Some(until) =
        db::self_organize::cooldown_active(pool, tier.guild_id, tier.id, caller_id).await?
    {
        return Ok(Some(SelfOrganizeBlock::OnCooldown { until }));
    }

    Ok(None)
}

/// Reasons HC->Run conversion can be refused for a self-organized HC.
/// Distinct from [`SelfOrganizeBlock`] because the convert path has a
/// different lifecycle (the HC already exists; we're gating the
/// transition out of it).
#[derive(Debug)]
pub enum SelfOrganizeConvertBlock {
    /// Not enough distinct reactors signed up.
    MinReactorsNotMet { observed: i64, required: i64 },
}

impl SelfOrganizeConvertBlock {
    pub fn user_message(&self) -> String {
        match self {
            SelfOrganizeConvertBlock::MinReactorsNotMet { observed, required } => format!(
                "This raid needs at least **{required}** reactor{plural} to start \
                 (currently **{observed}**). Wait for more signups.",
                plural = if *required == 1 { "" } else { "s" },
            ),
        }
    }
}

/// Gate the HC->Run conversion for a self-organized HC.
///
/// Counts distinct *non-bot* reactors across all reactions on the HC
/// message and rejects if fewer than `tier.self_organize_min_reactors`
/// signed up. The non-bot filter matters: the bot's own reactions on the
/// message (added by [`crate::services::reactions::attach_reactions`])
/// would otherwise count for one signup each.
pub fn check_can_convert(
    tier: &Tier,
    distinct_reactor_count: i64,
) -> Option<SelfOrganizeConvertBlock> {
    let required = tier.self_organize_min_reactors as i64;
    if distinct_reactor_count < required {
        return Some(SelfOrganizeConvertBlock::MinReactorsNotMet {
            observed: distinct_reactor_count,
            required,
        });
    }
    None
}

/// Set the post-cancel cooldown for a leader who cancelled their own
/// self-organized HC. The duration comes from the tier's
/// `self_organize_cancel_cooldown_seconds` knob.
pub async fn record_self_cancel(
    pool: &PgPool,
    tier: &Tier,
    user_id: i64,
) -> Result<()> {
    let duration = Duration::seconds(tier.self_organize_cancel_cooldown_seconds as i64);
    db::self_organize::cooldown_set(pool, tier.guild_id, tier.id, user_id, duration).await?;
    Ok(())
}

/// Displace a stale HC: best-effort edit the HC message to a closed
/// embed announcing the auto-cancel, then delete the HC + slot claim
/// in one transaction.
///
/// Discord-side errors (404 on the message, network failure) are
/// logged but don't fail the sweep — the row deletes happen
/// regardless, so the slot is freed even if the visual cleanup fails.
pub async fn sweep_stale_hc_for_slot(
    serenity_ctx: &serenity::Context,
    pool: &PgPool,
    claim: &SlotClaim,
    hc: &crate::db::models::Headcount,
    template: &DungeonTemplate,
) -> Result<()> {
    tracing::info!(
        hc_id = hc.id,
        guild_id = claim.guild_id,
        tier_id = claim.tier_id,
        dungeon_template_id = claim.dungeon_template_id,
        "sweeping stale self-organize headcount",
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

    // Release the claim *before* the HC delete: the FK is ON DELETE
    // NO ACTION, so a (NULL, NULL) intermediate state would violate the
    // table's CHECK constraint.
    let mut tx = pool.begin().await?;
    db::self_organize::claim_release_by_headcount(&mut tx, hc.id).await?;
    db::headcount::delete_tx(&mut tx, hc.id).await?;
    tx.commit().await?;

    Ok(())
}
