use poise::serenity_prelude as serenity;
use serenity::{
    ButtonStyle, CreateActionRow, CreateButton, CreateInteractionResponse,
    CreateInteractionResponseMessage, EditMessage, MessageId, ReactionType,
};

use crate::{db, embeds, services, BotData, BotError};

/// Entry point for all component interactions.  Only handles custom_ids
/// with the `hc:` prefix; all others are silently ignored (e.g. `setup:*`
/// clicks handled by the wizard's own collector).
pub async fn handle(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
) -> Result<(), BotError> {
    let id = &mci.data.custom_id;

    if id.starts_with("run:") {
        return super::run::handle_component(ctx, mci, data).await;
    }
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
        "confirm" if parts.len() >= 4 => {
            let reaction_id: i32 = match parts[3].parse() {
                Ok(n) => n,
                Err(_) => return Ok(()),
            };
            handle_confirm_click(ctx, mci, data, hc_id, reaction_id).await?;
        }
        "confirm_cancel" => handle_confirm_cancel_click(ctx, mci).await?,
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
    let bag_tiers = db::loot::list_bag_tiers(pool).await?;
    let threshold = db::loot::get_threshold(pool, hc.guild_id, hc.dungeon_template_id).await?;

    let (embed, components) = embeds::headcount::build(
        hc.id,
        &template,
        &reactions,
        &counts,
        &emoji_map,
        hc.leader_user_id as u64,
        &bag_tiers,
        &threshold,
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
        // Ephemeral confirm step — better UX than a modal form for what is
        // effectively a yes/no question. The confirm button records the
        // reaction and edits the headcount message in-place.
        let confirm = CreateButton::new(format!("hc:{hc_id}:confirm:{reaction_id}"))
            .label("Confirm")
            .emoji(ReactionType::Unicode("✅".into()))
            .style(ButtonStyle::Success);
        let cancel = CreateButton::new("hc:0:confirm_cancel")
            .label("Cancel")
            .style(ButtonStyle::Secondary);
        mci.create_response(
            ctx,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(format!(
                        "Confirm you're bringing **{}**? Only click Confirm if you actually have it.",
                        reaction.display_name
                    ))
                    .components(vec![CreateActionRow::Buttons(vec![confirm, cancel])])
                    .ephemeral(true),
            ),
        )
        .await?;
    } else {
        db::headcount::add_reaction(&data.db, hc_id, reaction_id, user_id, false).await?;
        rebuild_and_update(ctx, mci, data, &hc).await?;
    }

    Ok(())
}

async fn handle_confirm_click(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    hc_id: i32,
    reaction_id: i32,
) -> Result<(), BotError> {
    let Some(hc) = load_active(ctx, mci, data, hc_id).await? else {
        return Ok(());
    };

    let user_id = mci.user.id.get() as i64;
    db::headcount::add_reaction(&data.db, hc_id, reaction_id, user_id, true).await?;

    // Rebuild the headcount embed and edit the original message via HTTP —
    // this interaction targets the user's ephemeral, not the headcount.
    let pool = &data.db;
    let template = db::dungeon::get_by_id(pool, hc.dungeon_template_id)
        .await?
        .ok_or_else(|| format!("template {} not found", hc.dungeon_template_id))?;
    let reactions = db::dungeon::get_reactions(pool, hc.dungeon_template_id).await?;
    let counts = db::headcount::reaction_counts(pool, hc.id).await?;
    let emoji_map = db::emoji::get_all_as_map(pool).await?;
    let bag_tiers = db::loot::list_bag_tiers(pool).await?;
    let threshold = db::loot::get_threshold(pool, hc.guild_id, hc.dungeon_template_id).await?;

    let (embed, components) = embeds::headcount::build(
        hc.id,
        &template,
        &reactions,
        &counts,
        &emoji_map,
        hc.leader_user_id as u64,
        &bag_tiers,
        &threshold,
    );

    serenity::ChannelId::new(hc.channel_id as u64)
        .edit_message(
            &ctx.http,
            MessageId::new(hc.message_id as u64),
            EditMessage::new().add_embed(embed).components(components),
        )
        .await?;

    mci.create_response(
        ctx,
        CreateInteractionResponse::UpdateMessage(
            CreateInteractionResponseMessage::new()
                .content("✅ Confirmed!")
                .components(vec![]),
        ),
    )
    .await?;

    Ok(())
}

async fn handle_confirm_cancel_click(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
) -> Result<(), BotError> {
    mci.create_response(
        ctx,
        CreateInteractionResponse::UpdateMessage(
            CreateInteractionResponseMessage::new()
                .content("No changes made.")
                .components(vec![]),
        ),
    )
    .await?;
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

    let template = db::dungeon::get_by_id(&data.db, hc.dungeon_template_id)
        .await?
        .ok_or_else(|| format!("template {} not found", hc.dungeon_template_id))?;
    let tier = db::tier::get_by_id(&data.db, hc.tier_id)
        .await?
        .ok_or_else(|| format!("tier {} not found", hc.tier_id))?;

    // Pick the run channel: prefer the tier's unified runs channel, fall
    // back to whichever channel the headcount was posted in so a
    // partially-configured tier still works.
    let raid_channel_id = tier.runs_channel().unwrap_or(hc.channel_id);

    // Respond immediately so the click doesn't time out while we post the
    // run message. Close the headcount embed in the same response.
    let emoji_map = db::emoji::get_all_as_map(&data.db).await?;
    let closed_embed = embeds::headcount::build_closed(
        &template,
        &emoji_map,
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

    // Flip the headcount status *before* posting the run: if the message
    // post fails we still have a non-active headcount, and a retry won't
    // double-post.
    db::headcount::set_status(&data.db, hc_id, "converted").await?;

    services::raid::start_run(
        ctx,
        &data.db,
        hc.guild_id,
        &tier,
        &template,
        raid_channel_id,
        hc.leader_user_id,
        Some(hc.id),
    )
    .await?;

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

    let closed_embed = embeds::headcount::build_closed(
        &template,
        &emoji_map,
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
