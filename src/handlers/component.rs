use poise::serenity_prelude as serenity;

use crate::{BotData, BotError};

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
    if id.starts_with("hc:") {
        return super::headcount::handle_component(ctx, mci, data).await;
    }
    Ok(())
}
