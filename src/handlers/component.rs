use poise::serenity_prelude as serenity;

use crate::{BotData, BotError};

/// Entry point for all component interactions. Routes `hc:*`, `run:*`,
/// and `verify:*`. Other prefixes (e.g. `setup:*` from the /setup
/// wizard's own collector) are silently ignored here.
#[tracing::instrument(
    name = "component",
    skip_all,
    fields(
        custom_id = %mci.data.custom_id,
        user_id = mci.user.id.get(),
        guild_id = mci.guild_id.map(|g| g.get()),
    ),
)]
pub async fn handle(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
) -> Result<(), BotError> {
    let id = &mci.data.custom_id;

    if id.starts_with("run:") {
        return super::run::handle_component(ctx, mci, data).await;
    }
    if id.starts_with("hc:") {
        return super::headcount::handle_component(ctx, mci, data).await;
    }
    if id.starts_with("srui:") {
        return super::start_run_ui::handle_component(ctx, mci, data).await;
    }
    if id.starts_with("verify:") {
        return super::verify::handle_component(ctx, mci, data).await;
    }
    if id.starts_with("de:") {
        return super::dungeon_edit::handle_component(ctx, mci, data).await;
    }
    Ok(())
}
