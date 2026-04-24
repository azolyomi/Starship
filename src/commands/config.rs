use poise::CreateReply;

use crate::{
    db,
    services::permission::{self as perm_svc, Action},
    BotContext, BotError,
};

async fn autocomplete_dungeon<'a>(
    ctx: BotContext<'_>,
    partial: &'a str,
) -> impl Iterator<Item = String> + 'a {
    let guild_id = match ctx.guild_id() {
        Some(id) => id.get() as i64,
        None => return Vec::new().into_iter(),
    };
    db::dungeon::list_for_guild(&ctx.data().db, guild_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(move |d| d.name.to_lowercase().contains(&partial.to_lowercase()))
        .map(|d| d.name)
        .collect::<Vec<_>>()
        .into_iter()
}

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
    subcommands("threshold"),
    subcommand_required
)]
pub async fn config(_ctx: BotContext<'_>) -> Result<(), BotError> {
    Ok(())
}

/// Set the minimum loot bag tier shown in this dungeon's embeds.
///
/// Bag tiers (low to high): brown, pink, purple, cyan, blue, orange, red, white.
/// The renderer shows every bag tier at or above the threshold. Default is `white`
/// (strictest — only white-bag drops render).
#[poise::command(slash_command, guild_only, rename = "threshold")]
pub async fn threshold(
    ctx: BotContext<'_>,
    #[description = "Dungeon to configure"]
    #[autocomplete = "autocomplete_dungeon"]
    dungeon: String,
    #[description = "Lowest bag tier to show (default: white)"]
    #[autocomplete = "autocomplete_bag_tier"]
    tier: String,
) -> Result<(), BotError> {
    perm_svc::require(ctx, Action::ConfigureGuild, None, None).await?;

    let guild_id = ctx.guild_id().unwrap().get() as i64;
    let pool = &ctx.data().db;

    let Some(template) = db::dungeon::get_by_name(pool, guild_id, &dungeon).await? else {
        ctx.send(ephemeral(format!("Unknown dungeon `{dungeon}`.")))
            .await?;
        return Ok(());
    };

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

    db::loot::set_threshold(pool, guild_id, template.id, &tier).await?;

    ctx.send(ephemeral(format!(
        "Loot threshold for **{}** set to `{tier}`.",
        template.display_name
    )))
    .await?;

    Ok(())
}
