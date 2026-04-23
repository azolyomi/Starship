use crate::{BotContext, BotError};

/// Initial setup wizard for this server.
#[poise::command(slash_command, guild_only)]
pub async fn setup(ctx: BotContext<'_>) -> Result<(), BotError> {
    ctx.say("Setup coming soon!").await?;
    Ok(())
}
