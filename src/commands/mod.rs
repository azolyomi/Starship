pub mod dungeon;
pub mod notifications;
pub mod permission;
pub mod setup;
pub mod tier;

use crate::{BotData, BotError};

pub fn all() -> Vec<poise::Command<BotData, BotError>> {
    vec![
        setup::setup(),
        dungeon::dungeon(),
        permission::permission(),
        tier::tier(),
        notifications::notifications(),
    ]
}
