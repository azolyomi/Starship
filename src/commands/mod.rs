pub mod config;
pub mod dungeon;
pub mod headcount;
pub mod permission;
pub mod pingroles;
pub mod setup;
pub mod tier;
pub mod upload_emoji;

use crate::{BotData, BotError};

pub fn all() -> Vec<poise::Command<BotData, BotError>> {
    vec![
        setup::setup(),
        config::config(),
        dungeon::dungeon(),
        headcount::headcount(),
        permission::permission(),
        tier::tier(),
        pingroles::pingroles(),
        pingroles::pingroles_admin(),
        upload_emoji::upload_emoji(),
    ]
}
