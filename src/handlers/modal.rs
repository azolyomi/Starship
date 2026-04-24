use poise::serenity_prelude as serenity;

use crate::{BotData, BotError};

/// Entry point for modal submit interactions. Currently there are no modal
/// flows; the headcount confirmation modal was replaced with a one-click
/// confirm. Kept as a stub so `main.rs` can route future modals here.
pub async fn handle(
    _ctx: &serenity::Context,
    _modal: &serenity::ModalInteraction,
    _data: &BotData,
) -> Result<(), BotError> {
    Ok(())
}
