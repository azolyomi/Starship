use poise::serenity_prelude as serenity;
use serenity::{
    CreateInteractionResponse, CreateInteractionResponseMessage, EditMessage, MessageId,
};

use crate::{db, embeds, BotData, BotError};

/// Entry point for all modal submit interactions.
pub async fn handle(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    data: &BotData,
) -> Result<(), BotError> {
    let id = &modal.data.custom_id;

    if id.starts_with("hc:") {
        let parts: Vec<&str> = id.split(':').collect();
        // Expected: hc:<hc_id>:confirm:<reaction_id>
        if parts.len() >= 4 && parts[2] == "confirm" {
            let hc_id: i32 = match parts[1].parse() {
                Ok(n) => n,
                Err(_) => return Ok(()),
            };
            let reaction_id: i32 = match parts[3].parse() {
                Ok(n) => n,
                Err(_) => return Ok(()),
            };
            handle_confirm(ctx, modal, data, hc_id, reaction_id).await?;
        }
    }

    Ok(())
}

async fn handle_confirm(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    data: &BotData,
    hc_id: i32,
    reaction_id: i32,
) -> Result<(), BotError> {
    let pool = &data.db;

    // Validate headcount is still active.
    let Some(hc) = db::headcount::get(pool, hc_id).await? else {
        modal
            .create_response(
                ctx,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("Headcount not found.")
                        .ephemeral(true),
                ),
            )
            .await?;
        return Ok(());
    };
    if hc.status != "active" {
        modal
            .create_response(
                ctx,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("This headcount is no longer active.")
                        .ephemeral(true),
                ),
            )
            .await?;
        return Ok(());
    }

    let user_id = modal.user.id.get() as i64;

    // Record the confirmation.
    db::headcount::add_reaction(pool, hc_id, reaction_id, user_id, true).await?;

    // Rebuild the embed data.
    let template = db::dungeon::get_by_id(pool, hc.dungeon_template_id)
        .await?
        .ok_or_else(|| format!("template {} not found", hc.dungeon_template_id))?;
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

    // Acknowledge the modal with a silent ephemeral, then edit the headcount message directly.
    // (Modal interactions cannot use UpdateMessage to edit a different message.)
    modal
        .create_response(
            ctx,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("✅ Your confirmation has been recorded!")
                    .ephemeral(true),
            ),
        )
        .await?;

    serenity::ChannelId::new(hc.channel_id as u64)
        .edit_message(
            &ctx.http,
            MessageId::new(hc.message_id as u64),
            EditMessage::new().add_embed(embed).components(components),
        )
        .await?;

    Ok(())
}
