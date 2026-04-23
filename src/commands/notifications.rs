use crate::{
    services::permission::{self as perm_svc, Action},
    BotContext, BotError,
};

/// Post the dungeon notification role-selection message in the notification channel.
#[poise::command(slash_command, guild_only)]
pub async fn notifications(ctx: BotContext<'_>) -> Result<(), BotError> {
    perm_svc::require(ctx, Action::ConfigureGuild, None, None).await?;

    // TODO Phase 4: build the notification embed + role-toggle buttons and post
    // them to guild.notification_channel_id. The custom_id routing is:
    //   notify:<guild_id>:<dungeon_template_id>
    ctx.say("Notification role-selection message coming in Phase 4.")
        .await?;
    Ok(())
}
