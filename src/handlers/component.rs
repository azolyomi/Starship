use poise::serenity_prelude as serenity;
use serenity::{
    CreateActionRow, CreateInputText, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateModal, InputTextStyle,
};

use crate::{db, embeds, BotData, BotError};

/// Entry point for all component interactions.  Only handles custom_ids
/// with the `hc:` prefix; all others are silently ignored (e.g. `setup:*`
/// clicks handled by the wizard's own collector).
pub async fn handle(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
) -> Result<(), BotError> {
    let id = &mci.data.custom_id;

    if !id.starts_with("hc:") {
        return Ok(());
    }

    let parts: Vec<&str> = id.split(':').collect();
    if parts.len() < 3 {
        return Ok(());
    }

    let hc_id: i32 = match parts[1].parse() {
        Ok(n) => n,
        Err(_) => return Ok(()),
    };

    match parts[2] {
        "react" if parts.len() >= 4 => {
            let reaction_id: i32 = match parts[3].parse() {
                Ok(n) => n,
                Err(_) => return Ok(()),
            };
            handle_react(ctx, mci, data, hc_id, reaction_id).await?;
        }
        "start" => handle_start(ctx, mci, data, hc_id).await?,
        "cancel" => handle_cancel(ctx, mci, data, hc_id).await?,
        _ => {}
    }

    Ok(())
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

/// Load a headcount and validate it is active.  Sends an ephemeral error
/// and returns `None` if it isn't.
async fn load_active(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    hc_id: i32,
) -> Result<Option<crate::db::models::Headcount>, BotError> {
    match db::headcount::get(&data.db, hc_id).await? {
        None => {
            mci.create_response(ctx, ephemeral_msg("Headcount not found.")).await?;
            Ok(None)
        }
        Some(hc) if hc.status != "active" => {
            mci.create_response(ctx, ephemeral_msg("This headcount is no longer active."))
                .await?;
            Ok(None)
        }
        Some(hc) => Ok(Some(hc)),
    }
}

/// Re-fetch all headcount data and respond with an UpdateMessage so the
/// embed reflects the latest reaction counts.
async fn rebuild_and_update(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    hc: &crate::db::models::Headcount,
) -> Result<(), BotError> {
    let pool = &data.db;
    let template = db::dungeon::get_by_id(pool, hc.dungeon_template_id)
        .await?
        .ok_or_else(|| format!("dungeon template {} not found", hc.dungeon_template_id))?;
    let reactions = db::dungeon::get_reactions(pool, hc.dungeon_template_id).await?;
    let counts = db::headcount::reaction_counts(pool, hc.id).await?;
    let emoji_map = db::emoji::get_all_as_map(pool).await?;
    let tier = db::tier::get_by_id(pool, hc.tier_id)
        .await?
        .ok_or_else(|| format!("tier {} not found", hc.tier_id))?;

    let (embed, components) = embeds::headcount::build(
        hc.id,
        &template,
        &reactions,
        &counts,
        &emoji_map,
        hc.leader_user_id as u64,
        &tier.name,
    );

    mci.create_response(
        ctx,
        CreateInteractionResponse::UpdateMessage(
            CreateInteractionResponseMessage::new()
                .add_embed(embed)
                .components(components),
        ),
    )
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Per-action handlers
// ---------------------------------------------------------------------------

async fn handle_react(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    hc_id: i32,
    reaction_id: i32,
) -> Result<(), BotError> {
    let Some(hc) = load_active(ctx, mci, data, hc_id).await? else {
        return Ok(());
    };

    let reactions = db::dungeon::get_reactions(&data.db, hc.dungeon_template_id).await?;
    let Some(reaction) = reactions.iter().find(|r| r.id == reaction_id) else {
        mci.create_response(ctx, ephemeral_msg("Unknown reaction.")).await?;
        return Ok(());
    };

    let user_id = mci.user.id.get() as i64;
    let existing =
        db::headcount::get_user_reaction(&data.db, hc_id, reaction_id, user_id).await?;

    if existing.is_some() {
        // Toggle off — remove the reaction and refresh the embed.
        db::headcount::remove_reaction(&data.db, hc_id, reaction_id, user_id).await?;
        rebuild_and_update(ctx, mci, data, &hc).await?;
    } else if reaction.requires_confirmation {
        // Open a confirmation modal; the embed is updated when the modal is submitted.
        let modal = CreateModal::new(
            format!("hc:{hc_id}:confirm:{reaction_id}"),
            format!("Confirm: {}", reaction.display_name),
        )
        .components(vec![CreateActionRow::InputText(
            CreateInputText::new(
                InputTextStyle::Short,
                "Details (optional)",
                "details",
            )
            .placeholder("Screenshot URL, item description, etc.")
            .required(false)
            .max_length(200),
        )]);
        mci.create_response(ctx, CreateInteractionResponse::Modal(modal)).await?;
    } else {
        db::headcount::add_reaction(&data.db, hc_id, reaction_id, user_id, false).await?;
        rebuild_and_update(ctx, mci, data, &hc).await?;
    }

    Ok(())
}

async fn handle_start(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    hc_id: i32,
) -> Result<(), BotError> {
    let Some(hc) = load_active(ctx, mci, data, hc_id).await? else {
        return Ok(());
    };

    if mci.user.id.get() as i64 != hc.leader_user_id {
        mci.create_response(ctx, ephemeral_msg("Only the headcount leader can start the run."))
            .await?;
        return Ok(());
    }

    db::headcount::set_status(&data.db, hc_id, "converted").await?;

    let template = db::dungeon::get_by_id(&data.db, hc.dungeon_template_id)
        .await?
        .ok_or_else(|| format!("template {} not found", hc.dungeon_template_id))?;
    let emoji_map = db::emoji::get_all_as_map(&data.db).await?;
    let tier = db::tier::get_by_id(&data.db, hc.tier_id)
        .await?
        .ok_or_else(|| format!("tier {} not found", hc.tier_id))?;

    let closed_embed = embeds::headcount::build_closed(
        &template,
        &emoji_map,
        &tier.name,
        &format!("Run started by <@{}>! Watch for the run message.", hc.leader_user_id),
        false,
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

    // TODO Phase 5: create the run message here.

    Ok(())
}

async fn handle_cancel(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    hc_id: i32,
) -> Result<(), BotError> {
    let Some(hc) = load_active(ctx, mci, data, hc_id).await? else {
        return Ok(());
    };

    let user_id = mci.user.id.get() as i64;

    // Only the leader can cancel for now; permission-based cancel comes in a later phase.
    if user_id != hc.leader_user_id {
        mci.create_response(
            ctx,
            ephemeral_msg("Only the headcount leader can cancel this headcount."),
        )
        .await?;
        return Ok(());
    }

    db::headcount::set_status(&data.db, hc_id, "cancelled").await?;

    let template = db::dungeon::get_by_id(&data.db, hc.dungeon_template_id)
        .await?
        .ok_or_else(|| format!("template {} not found", hc.dungeon_template_id))?;
    let emoji_map = db::emoji::get_all_as_map(&data.db).await?;
    let tier = db::tier::get_by_id(&data.db, hc.tier_id)
        .await?
        .ok_or_else(|| format!("tier {} not found", hc.tier_id))?;

    let closed_embed = embeds::headcount::build_closed(
        &template,
        &emoji_map,
        &tier.name,
        &format!("Headcount cancelled by <@{user_id}>."),
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
