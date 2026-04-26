//! Component + modal handling for `hc:*` custom_ids.
//!
//! Button flow:
//!   hc:<id>:start   -- verify reactions, then open the start-run modal
//!   hc:<id>:cancel  -- grey the embed, delete headcount row
//!   hc:<id>:confirmstart (modal submit)
//!                   -- create the run with modal-supplied location/party

use std::collections::HashMap;

use poise::serenity_prelude as serenity;
use serenity::{
    ActionRowComponent, CreateActionRow, CreateInputText, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateModal, EditMessage, InputTextStyle, MessageId,
};

use crate::db::models::{BotEmoji, DungeonReaction, Headcount};
use crate::embeds::headcount::emoji_rt;
use crate::services::reactions::reaction_types_match;
use crate::{db, embeds, services, BotData, BotError};

// ---------------------------------------------------------------------------
// Component dispatcher
// ---------------------------------------------------------------------------

pub async fn handle_component(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
) -> Result<(), BotError> {
    let parts: Vec<&str> = mci.data.custom_id.split(':').collect();
    if parts.len() < 3 {
        return Ok(());
    }
    let hc_id: i32 = match parts[1].parse() {
        Ok(n) => n,
        Err(_) => return Ok(()),
    };
    match parts[2] {
        "start" => handle_start(ctx, mci, data, hc_id).await,
        "cancel" => handle_cancel(ctx, mci, data, hc_id).await,
        _ => Ok(()),
    }
}

pub async fn handle_modal(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    data: &BotData,
) -> Result<(), BotError> {
    let parts: Vec<&str> = modal.data.custom_id.split(':').collect();
    if parts.len() < 3 {
        return Ok(());
    }
    let hc_id: i32 = match parts[1].parse() {
        Ok(n) => n,
        Err(_) => return Ok(()),
    };
    if parts[2] == "confirmstart" {
        handle_confirm_start(ctx, modal, data, hc_id).await
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ephemeral_msg(text: impl Into<String>) -> CreateInteractionResponse {
    CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new()
            .content(text)
            .ephemeral(true),
    )
}

async fn load_active_for_component(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    hc_id: i32,
) -> Result<Option<Headcount>, BotError> {
    match db::headcount::get(&data.db, hc_id).await? {
        None => {
            mci.create_response(ctx, ephemeral_msg("This headcount is no longer active."))
                .await?;
            Ok(None)
        }
        Some(hc) => Ok(Some(hc)),
    }
}

async fn require_organizer_component(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    hc: &Headcount,
) -> Result<bool, BotError> {
    let ok = services::permission::can_organize_from_interaction(
        &data.db,
        hc.guild_id,
        mci,
        hc.leader_user_id,
        Some(hc.tier_id),
        Some(hc.dungeon_template_id),
    )
    .await?;
    if !ok {
        mci.create_response(
            ctx,
            ephemeral_msg("Only the raid leader or users with **ManageRuns** can do that."),
        )
        .await?;
    }
    Ok(ok)
}

/// Pull a single input value off a modal submission by custom_id.
fn extract_input(modal: &serenity::ModalInteraction, custom_id: &str) -> Option<String> {
    for row in &modal.data.components {
        for comp in &row.components {
            if let ActionRowComponent::InputText(it) = comp {
                if it.custom_id == custom_id {
                    return it.value.clone();
                }
            }
        }
    }
    None
}

/// Fetch the headcount message and check each required reaction has at
/// least one non-bot reactor. Returns the display names of the reactions
/// that still need a reactor.
async fn missing_reactions(
    ctx: &serenity::Context,
    hc: &Headcount,
    reactions_list: &[DungeonReaction],
    emoji_map: &HashMap<String, BotEmoji>,
) -> Result<Vec<String>, BotError> {
    let channel_id = serenity::ChannelId::new(hc.channel_id as u64);
    let message_id = MessageId::new(hc.message_id as u64);
    let msg = channel_id.message(&ctx.http, message_id).await?;

    let mut missing = Vec::new();
    for required in reactions_list {
        // num_required <= 0 means the reaction renders + is clickable but
        // does not gate /start (e.g. an optional key on a low-key dungeon).
        if required.num_required <= 0 {
            continue;
        }
        let Some(required_rt) = emoji_rt(&required.emoji, emoji_map) else {
            // Emoji not resolvable (logical name never uploaded). Don't block
            // the raid on something the bot couldn't render in the first
            // place — just skip.
            continue;
        };
        let has_reactor = msg.reactions.iter().any(|mr| {
            reaction_types_match(&mr.reaction_type, &required_rt)
                && mr.count.saturating_sub(if mr.me { 1 } else { 0 }) >= 1
        });
        if !has_reactor {
            missing.push(required.display_name.clone());
        }
    }
    Ok(missing)
}

// ---------------------------------------------------------------------------
// Button handlers
// ---------------------------------------------------------------------------

async fn handle_start(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    hc_id: i32,
) -> Result<(), BotError> {
    let Some(hc) = load_active_for_component(ctx, mci, data, hc_id).await? else {
        return Ok(());
    };
    if !require_organizer_component(ctx, mci, data, &hc).await? {
        return Ok(());
    }

    let reactions_list = db::dungeon::get_reactions(&data.db, hc.dungeon_template_id).await?;
    let emoji_map = db::emoji::get_all_as_map(&data.db).await?;

    let missing = missing_reactions(ctx, &hc, &reactions_list, &emoji_map).await?;
    if !missing.is_empty() {
        mci.create_response(
            ctx,
            ephemeral_msg(format!(
                "Can't start yet — still waiting for at least one reactor on: {}.",
                missing.join(", "),
            )),
        )
        .await?;
        return Ok(());
    }

    // Open the start-run modal so the leader can confirm / edit location and
    // party before the run message goes out.
    let rows = vec![
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Short, "Location", "location")
                .placeholder("e.g. USW3 realm, nexus 5 o'clock")
                .value(hc.location.clone().unwrap_or_default())
                .required(false)
                .max_length(200),
        ),
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Paragraph, "Party", "party")
                .placeholder("Free-form: classes, roles, pairings…")
                .value(hc.party.clone().unwrap_or_default())
                .required(false)
                .max_length(1000),
        ),
    ];
    let modal = CreateModal::new(format!("hc:{hc_id}:confirmstart"), "Start run").components(rows);
    mci.create_response(ctx, CreateInteractionResponse::Modal(modal))
        .await?;
    Ok(())
}

async fn handle_cancel(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    hc_id: i32,
) -> Result<(), BotError> {
    let Some(hc) = load_active_for_component(ctx, mci, data, hc_id).await? else {
        return Ok(());
    };
    if !require_organizer_component(ctx, mci, data, &hc).await? {
        return Ok(());
    }

    // Atomic claim + slot-claim release in one tx. The release is a no-op
    // for HCs in non-self-organize tiers (no claim row exists), so this
    // path is uniform for both flows.
    let mut tx = data.db.begin().await?;
    db::self_organize::claim_release_by_headcount(&mut tx, hc_id).await?;
    let row_existed = db::headcount::delete_tx(&mut tx, hc_id).await?;
    if !row_existed {
        tx.rollback().await?;
        mci.create_response(ctx, ephemeral_msg("This headcount is no longer active."))
            .await?;
        return Ok(());
    }
    tx.commit().await?;

    let canceller_id = mci.user.id.get() as i64;

    // Self-cancel cooldown: only when a self-organized HC is cancelled by
    // its own leader. Staff overrides (ManageRuns) don't count — those are
    // a moderation action, not a misuse signal.
    if hc.is_self_organized && canceller_id == hc.leader_user_id {
        if let Some(tier) = db::tier::get_by_id(&data.db, hc.tier_id).await? {
            if let Err(e) =
                services::self_organize::record_self_cancel(&data.db, &tier, canceller_id).await
            {
                tracing::warn!(
                    error = ?e,
                    hc_id,
                    user_id = canceller_id,
                    tier_id = hc.tier_id,
                    "failed to record self-organize cancel cooldown",
                );
            }
        }
    }

    let template = db::dungeon::get_by_id(&data.db, hc.dungeon_template_id)
        .await?
        .ok_or_else(|| format!("template {} not found", hc.dungeon_template_id))?;
    let emoji_map = db::emoji::get_all_as_map(&data.db).await?;

    let closed_embed = embeds::headcount::build_closed(
        &template,
        &emoji_map,
        &format!("Headcount cancelled by <@{}>.", mci.user.id.get()),
        true,
    );

    mci.create_response(
        ctx,
        CreateInteractionResponse::UpdateMessage(
            CreateInteractionResponseMessage::new()
                .add_embed(closed_embed)
                .components(vec![]),
        ),
    )
    .await?;

    // Refresh the listing for self-organize tiers so the cancelled raid
    // disappears from the active-raids view. Best-effort.
    if let Ok(Some(tier)) = db::tier::get_by_id(&data.db, hc.tier_id).await {
        if tier.enable_self_organization {
            if let Err(e) =
                services::self_organize_listing::refresh_listing(ctx, &data.db, &tier).await
            {
                tracing::warn!(
                    error = ?e,
                    tier_id = tier.id,
                    "failed to refresh self-organize listing after HC cancel",
                );
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Modal submission
// ---------------------------------------------------------------------------

async fn handle_confirm_start(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    data: &BotData,
    hc_id: i32,
) -> Result<(), BotError> {
    let Some(hc) = db::headcount::get(&data.db, hc_id).await? else {
        modal
            .create_response(ctx, ephemeral_msg("This headcount is no longer active."))
            .await?;
        return Ok(());
    };

    // Organizer gate — reconstruct from the modal's member since we don't
    // have a ComponentInteraction shape here.
    let caller_id = modal.user.id.get() as i64;
    let (perms, roles) = match modal.member.as_ref() {
        Some(m) => (
            m.permissions,
            m.roles.iter().map(|r| r.get() as i64).collect(),
        ),
        None => (None, Vec::new()),
    };
    let ok = services::permission::can_organize(
        &data.db,
        hc.guild_id,
        caller_id,
        perms,
        &roles,
        hc.leader_user_id,
        Some(hc.tier_id),
        Some(hc.dungeon_template_id),
    )
    .await?;
    if !ok {
        modal
            .create_response(
                ctx,
                ephemeral_msg("Only the raid leader or users with **ManageRuns** can do that."),
            )
            .await?;
        return Ok(());
    }

    let template = db::dungeon::get_by_id(&data.db, hc.dungeon_template_id)
        .await?
        .ok_or_else(|| format!("template {} not found", hc.dungeon_template_id))?;
    let tier = db::tier::get_by_id(&data.db, hc.tier_id)
        .await?
        .ok_or_else(|| format!("tier {} not found", hc.tier_id))?;
    let raid_channel_id = tier.runs_channel_id.unwrap_or(hc.channel_id);

    // Pre-flight: refuse to convert if the destination runs channel is gone.
    // We check before claiming the headcount so a deleted channel doesn't
    // burn the headcount row — the organizer can still cancel cleanly.
    if !services::channels::channel_exists(
        &ctx.http,
        serenity::ChannelId::new(raid_channel_id as u64),
    )
    .await?
    {
        modal
            .create_response(
                ctx,
                ephemeral_msg(format!(
                    "Can't post the run — channel <#{raid_channel_id}> is gone. \
                     An admin needs to repoint the tier (`/tier edit` or `/setup`) \
                     before this headcount can convert."
                )),
            )
            .await?;
        return Ok(());
    }

    // Self-organize min-reactors gate. Only enforced for HCs that originated
    // from the self-organize button; staff `/headcount` in self-organize
    // tiers is already trust-gated by the StartHeadcount permission and
    // doesn't need the anti-troll floor. Organizers (ManageRuns / superadmin
    // / Discord admin) also bypass the floor — `check_can_convert` skips
    // the count comparison for trusted operators.
    if hc.is_self_organized {
        let is_org = services::permission::is_organizer_from_modal(
            &data.db,
            hc.guild_id,
            modal,
            Some(hc.tier_id),
        )
        .await?;
        let count = match services::reactions::count_distinct_non_bot_reactors(
            &ctx.http,
            serenity::ChannelId::new(hc.channel_id as u64),
            MessageId::new(hc.message_id as u64),
        )
        .await
        {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    error = ?e,
                    hc_id,
                    "failed to count reactors for min-reactors gate; proceeding without",
                );
                tier.self_organize_min_reactors as i64
            }
        };
        if let Some(block) = services::self_organize::check_can_convert(&tier, count, is_org) {
            modal
                .create_response(ctx, ephemeral_msg(block.user_message()))
                .await?;
            return Ok(());
        }
    }

    let location_raw = extract_input(modal, "location").unwrap_or_default();
    let location_trim = location_raw.trim();
    let location = if location_trim.is_empty() {
        None
    } else {
        Some(location_trim)
    };
    let party_raw = extract_input(modal, "party").unwrap_or_default();
    let party_trim = party_raw.trim();
    let party = if party_trim.is_empty() {
        None
    } else {
        Some(party_trim)
    };

    // Unified HC->Run convert. One transaction creates the run row,
    // migrates any slot claim off the HC, then deletes the HC. Order
    // matters: the slot_claim FK to headcounts is `ON DELETE NO ACTION`,
    // so the HC delete fails immediately if a claim still references it.
    // Both convert variants share this skeleton — the only difference is
    // whether we *swap* the claim to the new run (preserves the slot
    // lock through the convert) or *release* it (tier is no longer
    // self-organize, lock is meaningless).
    let mut tx = data.db.begin().await?;
    let mut run = db::run::create_tx(
        &mut tx,
        hc.guild_id,
        tier.id,
        template.id,
        raid_channel_id,
        hc.leader_user_id,
        template.requires_vc,
        hc.is_self_organized,
    )
    .await?;

    // Migrate or release the slot claim BEFORE deleting the HC. Both
    // helpers are no-ops when no claim row exists (e.g. HC was created
    // when the tier was non-SO); that's fine — without a referencing
    // claim there's nothing to violate the FK with either.
    if tier.enable_self_organization {
        db::self_organize::claim_swap_to_run(&mut tx, hc.id, run.id).await?;
    } else {
        // Stale claim defence: if the tier was SO at HC creation but
        // disabled before convert, a claim still references the HC.
        // Release it so the HC delete passes the FK check.
        db::self_organize::claim_release_by_headcount(&mut tx, hc.id).await?;
    }

    let row_existed = db::headcount::delete_tx(&mut tx, hc_id).await?;
    if !row_existed {
        tx.rollback().await?;
        modal
            .create_response(ctx, ephemeral_msg("This headcount is no longer active."))
            .await?;
        return Ok(());
    }
    tx.commit().await?;

    // Persist location/party as follow-up UPDATEs outside the tx (no
    // contention; Discord HTTP is already imminent).
    if let Some(loc) = location {
        db::run::set_location(&data.db, run.id, Some(loc)).await?;
        run.location = Some(loc.to_string());
    }
    if let Some(p) = party {
        db::run::set_party(&data.db, run.id, Some(p)).await?;
        run.party = Some(p.to_string());
    }

    modal
        .create_response(
            ctx,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(format!("Run posted in <#{raid_channel_id}>."))
                    .ephemeral(true),
            ),
        )
        .await?;

    if let Err(e) = serenity::ChannelId::new(hc.channel_id as u64)
        .edit_message(
            &ctx.http,
            MessageId::new(hc.message_id as u64),
            EditMessage::new().components(vec![]),
        )
        .await
    {
        tracing::warn!(
            error = ?e,
            hc_id,
            channel_id = hc.channel_id,
            message_id = hc.message_id,
            "failed to strip buttons from converted headcount message",
        );
    }

    services::raid::finalize_run_post_create(ctx, &data.db, &mut run, &template, raid_channel_id)
        .await?;

    // Listing refresh only matters when the tier is currently SO.
    if tier.enable_self_organization {
        if let Err(e) = services::self_organize_listing::refresh_listing(ctx, &data.db, &tier).await
        {
            tracing::warn!(
                error = ?e,
                tier_id = tier.id,
                "failed to refresh self-organize listing after HC convert",
            );
        }
    }

    Ok(())
}
