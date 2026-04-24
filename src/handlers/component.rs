use poise::serenity_prelude as serenity;
use serenity::{
    CreateInteractionResponse, CreateInteractionResponseMessage,
};

use crate::{db, embeds, services, BotData, BotError};

/// Entry point for all component interactions. Routes `hc:*` and `run:*`.
/// Other prefixes (e.g. `setup:*` from the /setup wizard's own collector)
/// are silently ignored here.
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

/// Shared organizer gate for headcount lifecycle buttons.
async fn require_organizer(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    hc: &crate::db::models::Headcount,
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

// ---------------------------------------------------------------------------
// Headcount lifecycle
// ---------------------------------------------------------------------------

async fn handle_start(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    hc_id: i32,
) -> Result<(), BotError> {
    let Some(hc) = load_active(ctx, mci, data, hc_id).await? else {
        return Ok(());
    };
    if !require_organizer(ctx, mci, data, &hc).await? {
        return Ok(());
    }

    let template = db::dungeon::get_by_id(&data.db, hc.dungeon_template_id)
        .await?
        .ok_or_else(|| format!("template {} not found", hc.dungeon_template_id))?;
    let tier = db::tier::get_by_id(&data.db, hc.tier_id)
        .await?
        .ok_or_else(|| format!("tier {} not found", hc.tier_id))?;

    // Prefer the tier's unified runs channel; fall back to wherever the
    // headcount was posted so a half-configured tier still works.
    let raid_channel_id = tier.runs_channel_id.unwrap_or(hc.channel_id);

    let emoji_map = db::emoji::get_all_as_map(&data.db).await?;
    let closed_embed = embeds::headcount::build_closed(
        &template,
        &emoji_map,
        &format!(
            "Run started by <@{}>! Watch for the run message.",
            mci.user.id.get()
        ),
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

    // Flip headcount status *before* posting the run: if the run-post fails
    // we still have a non-active headcount, and a retry won't double-post.
    db::headcount::set_status(&data.db, hc_id, "converted").await?;

    services::raid::start_run(
        ctx,
        &data.db,
        hc.guild_id,
        &tier,
        &template,
        raid_channel_id,
        // Leader of the run is whoever was leading the headcount. An
        // organizer who hits Start on someone else's headcount hands the
        // raid off — matches how a raid lead kicks off a run for a friend.
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
    if !require_organizer(ctx, mci, data, &hc).await? {
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
