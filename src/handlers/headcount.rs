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
            ephemeral_msg(
                "Only the raid leader or users with **ManageRuns** can do that.",
            ),
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

/// Compare two ReactionTypes for equality in a way that's useful for us:
/// custom emojis match by ID, unicode by string. Other variants never match.
fn reaction_types_match(a: &serenity::ReactionType, b: &serenity::ReactionType) -> bool {
    use serenity::ReactionType::*;
    match (a, b) {
        (Custom { id: ia, .. }, Custom { id: ib, .. }) => ia == ib,
        (Unicode(sa), Unicode(sb)) => sa == sb,
        _ => false,
    }
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
    let mut rows = Vec::new();
    rows.push(CreateActionRow::InputText(
        CreateInputText::new(InputTextStyle::Short, "Location", "location")
            .placeholder("e.g. USW3 realm, nexus 5 o'clock")
            .value(hc.location.clone().unwrap_or_default())
            .required(false)
            .max_length(200),
    ));
    rows.push(CreateActionRow::InputText(
        CreateInputText::new(InputTextStyle::Paragraph, "Party", "party")
            .placeholder("Free-form: classes, roles, pairings…")
            .value(hc.party.clone().unwrap_or_default())
            .required(false)
            .max_length(1000),
    ));
    let modal = CreateModal::new(format!("hc:{hc_id}:confirmstart"), "Start run")
        .components(rows);
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

    if !db::headcount::delete(&data.db, hc_id).await? {
        mci.create_response(ctx, ephemeral_msg("This headcount is no longer active."))
            .await?;
        return Ok(());
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
                ephemeral_msg(
                    "Only the raid leader or users with **ManageRuns** can do that.",
                ),
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

    // Atomic claim: first confirm wins.
    if !db::headcount::delete(&data.db, hc_id).await? {
        modal
            .create_response(ctx, ephemeral_msg("This headcount is no longer active."))
            .await?;
        return Ok(());
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

    // Ack privately first so Discord doesn't time out while we post the run.
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

    // Strip buttons off the original headcount message — it's no longer
    // actionable — but keep the embed intact so the thread stays readable.
    let _ = serenity::ChannelId::new(hc.channel_id as u64)
        .edit_message(
            &ctx.http,
            MessageId::new(hc.message_id as u64),
            EditMessage::new().components(vec![]),
        )
        .await;

    services::raid::start_run(
        ctx,
        &data.db,
        hc.guild_id,
        &tier,
        &template,
        raid_channel_id,
        hc.leader_user_id,
        location,
        party,
    )
    .await?;

    Ok(())
}
