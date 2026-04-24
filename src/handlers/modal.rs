use poise::serenity_prelude as serenity;

use crate::{BotData, BotError};

/// Entry point for modal submit interactions. Dispatches on the custom_id
/// prefix: `hc:*` (start-run confirmation) and `run:*` (location / party
/// edits from the control panel).
pub async fn handle(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    data: &BotData,
) -> Result<(), BotError> {
    let id = &modal.data.custom_id;
    if id.starts_with("run:") {
        return super::run::handle_modal(ctx, modal, data).await;
    }
    if id.starts_with("hc:") {
        return super::headcount::handle_modal(ctx, modal, data).await;
    }
    Ok(())
}
