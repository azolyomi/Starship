//! `/config` — guild-level configuration knobs.
//!
//! Six subcommands. `show` is anyone-readable; the rest are
//! ConfigureGuild-gated. They are deliberately one-shot setters intended
//! as a faster alternative to running the `/setup` wizard for a single
//! tweak. Clearing has its own dedicated subcommand per setting because
//! Discord's slash-command UI doesn't have a clean "pass nothing to
//! clear" idiom.

use poise::serenity_prelude as serenity;
use poise::CreateReply;
use serenity::{CreateEmbed, GuildChannel};

use crate::{
    db, guild_id_i64,
    services::permission::{self as perm_svc, Action},
    BotContext, BotError,
};

async fn autocomplete_bag_tier<'a>(
    ctx: BotContext<'_>,
    partial: &'a str,
) -> impl Iterator<Item = String> + 'a {
    db::loot::list_bag_tiers(&ctx.data().db)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(move |t| t.name.to_lowercase().contains(&partial.to_lowercase()))
        .map(|t| t.name)
        .collect::<Vec<_>>()
        .into_iter()
}

fn ephemeral(msg: impl Into<String>) -> CreateReply {
    CreateReply::default().content(msg).ephemeral(true)
}

/// Guild-level configuration.
#[poise::command(
    slash_command,
    guild_only,
    subcommands(
        "show",
        "threshold",
        "log_channel",
        "log_channel_clear",
        "superadmin",
        "superadmin_clear"
    ),
    subcommand_required,
    default_member_permissions = "MANAGE_GUILD"
)]
pub async fn config(_ctx: BotContext<'_>) -> Result<(), BotError> {
    Ok(())
}

/// Show the current guild configuration.
#[poise::command(slash_command, guild_only)]
pub async fn show(ctx: BotContext<'_>) -> Result<(), BotError> {
    let guild_id = guild_id_i64(ctx);
    let pool = &ctx.data().db;

    // `upsert` (not `get`) so a fresh guild that hasn't run /setup yet
    // still gets a sensible "everything unset" view.
    let guild = db::guild::upsert(pool, guild_id).await?;
    let tiers = db::tier::list(pool, guild_id).await?;

    let log_line = match guild.log_channel_id {
        Some(id) => format!("<#{id}>"),
        None => "_unset_".to_string(),
    };
    let admin_line = match guild.superadmin_user_id {
        Some(id) => format!("<@{id}>"),
        None => "_unset_".to_string(),
    };
    let tiers_line = if tiers.is_empty() {
        "_no tiers — run `/setup` first_".to_string()
    } else {
        tiers
            .iter()
            .map(|t| match t.runs_channel_id {
                Some(c) => format!("• **{}** → <#{c}>", t.name),
                None => format!("• **{}** → _no runs channel_", t.name),
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let embed = CreateEmbed::default()
        .title("Guild config")
        .color(0x5865F2u32)
        .field(
            "Loot threshold",
            format!("`{}`", guild.loot_tier_threshold),
            true,
        )
        .field("Log channel", log_line, true)
        .field("Superadmin", admin_line, true)
        .field("Tiers", tiers_line, false)
        .footer(serenity::CreateEmbedFooter::new(
            "Use `/config <setting>` to change · `/setup` for the full wizard",
        ));

    ctx.send(CreateReply::default().embed(embed).ephemeral(true))
        .await?;
    Ok(())
}

/// Set the minimum loot bag tier shown in run and headcount embeds.
///
/// Bag tiers (low to high): brown, pink, purple, cyan, blue, orange, red, white.
/// The renderer shows every bag tier at or above the threshold. Default is `white`
/// (strictest — only white-bag drops render).
#[poise::command(slash_command, guild_only, rename = "threshold")]
pub async fn threshold(
    ctx: BotContext<'_>,
    #[description = "Lowest bag tier to show (default: white)"]
    #[autocomplete = "autocomplete_bag_tier"]
    tier: String,
) -> Result<(), BotError> {
    perm_svc::require(ctx, Action::ConfigureGuild, None, None).await?;

    let guild_id = guild_id_i64(ctx);
    let pool = &ctx.data().db;

    let bag_tiers = db::loot::list_bag_tiers(pool).await?;
    if !bag_tiers.iter().any(|t| t.name == tier) {
        let names: Vec<&str> = bag_tiers.iter().map(|t| t.name.as_str()).collect();
        ctx.send(ephemeral(format!(
            "Unknown bag tier `{tier}`. Valid tiers: {}.",
            names.join(", ")
        )))
        .await?;
        return Ok(());
    }

    db::loot::set_threshold(pool, guild_id, &tier).await?;

    ctx.send(ephemeral(format!(
        "Loot threshold set to `{tier}` for this guild."
    )))
    .await?;

    Ok(())
}

/// Set the audit log channel — where run lifecycle events are posted.
#[poise::command(slash_command, guild_only, rename = "log-channel")]
pub async fn log_channel(
    ctx: BotContext<'_>,
    #[description = "Text channel to post audit log entries to"] channel: GuildChannel,
) -> Result<(), BotError> {
    perm_svc::require(ctx, Action::ConfigureGuild, None, None).await?;

    let guild_id = guild_id_i64(ctx);
    let pool = &ctx.data().db;

    if !matches!(
        channel.kind,
        serenity::ChannelType::Text | serenity::ChannelType::News
    ) {
        ctx.send(ephemeral(
            "Pick a regular text or announcement channel for the log channel.",
        ))
        .await?;
        return Ok(());
    }

    db::guild::upsert(pool, guild_id).await?;
    db::guild::set_log_channel(pool, guild_id, Some(channel.id.get() as i64)).await?;

    ctx.send(ephemeral(format!("Log channel set to <#{}>.", channel.id)))
        .await?;
    Ok(())
}

/// Clear the audit log channel — disables run lifecycle audit posts.
#[poise::command(slash_command, guild_only, rename = "log-channel-clear")]
pub async fn log_channel_clear(ctx: BotContext<'_>) -> Result<(), BotError> {
    perm_svc::require(ctx, Action::ConfigureGuild, None, None).await?;

    let guild_id = guild_id_i64(ctx);
    let pool = &ctx.data().db;

    db::guild::upsert(pool, guild_id).await?;
    db::guild::set_log_channel(pool, guild_id, None).await?;

    ctx.send(ephemeral("Log channel cleared.")).await?;
    Ok(())
}

/// Set the per-guild superadmin — bypasses every permission check in this guild.
#[poise::command(slash_command, guild_only)]
pub async fn superadmin(
    ctx: BotContext<'_>,
    #[description = "User who should bypass all permission checks in this guild"]
    user: serenity::User,
) -> Result<(), BotError> {
    perm_svc::require(ctx, Action::ConfigureGuild, None, None).await?;

    let guild_id = guild_id_i64(ctx);
    let pool = &ctx.data().db;

    if user.bot {
        ctx.send(ephemeral("Pick a real user, not a bot.")).await?;
        return Ok(());
    }

    db::guild::upsert(pool, guild_id).await?;
    db::guild::set_superadmin(pool, guild_id, Some(user.id.get() as i64)).await?;

    ctx.send(ephemeral(format!("Superadmin set to <@{}>.", user.id)))
        .await?;
    Ok(())
}

/// Clear the per-guild superadmin.
#[poise::command(slash_command, guild_only, rename = "superadmin-clear")]
pub async fn superadmin_clear(ctx: BotContext<'_>) -> Result<(), BotError> {
    perm_svc::require(ctx, Action::ConfigureGuild, None, None).await?;

    let guild_id = guild_id_i64(ctx);
    let pool = &ctx.data().db;

    db::guild::upsert(pool, guild_id).await?;
    db::guild::set_superadmin(pool, guild_id, None).await?;

    ctx.send(ephemeral("Superadmin cleared.")).await?;
    Ok(())
}
