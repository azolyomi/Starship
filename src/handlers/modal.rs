use poise::serenity_prelude as serenity;

use crate::{BotData, BotError};

/// Entry point for modal submit interactions. Dispatches on the custom_id
/// prefix — currently only `run:*` has modal flows (location / party).
pub async fn handle(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    data: &BotData,
) -> Result<(), BotError> {
    if modal.data.custom_id.starts_with("run:") {
        return super::run::handle_modal(ctx, modal, data).await;
    }
    Ok(())
}
