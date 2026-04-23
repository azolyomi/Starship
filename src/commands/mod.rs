pub mod setup;

use crate::{BotData, BotError};

pub fn all() -> Vec<poise::Command<BotData, BotError>> {
    vec![setup::setup()]
}
