use poise::serenity_prelude as serenity;

use crate::{
    db,
    services::permission::{self as perm_svc, Action},
    BotContext, BotError,
};

async fn autocomplete_tier<'a>(
    ctx: BotContext<'_>,
    partial: &'a str,
) -> impl Iterator<Item = String> + 'a {
    let guild_id = match ctx.guild_id() {
        Some(id) => id.get() as i64,
        None => return Vec::new().into_iter(),
    };
    let tiers = db::tier::list(&ctx.data().db, guild_id)
        .await
        .unwrap_or_default();
    tiers
        .into_iter()
        .filter(move |t| t.name.to_lowercase().contains(&partial.to_lowercase()))
        .map(|t| t.name)
        .collect::<Vec<_>>()
        .into_iter()
}

async fn autocomplete_dungeon<'a>(
    ctx: BotContext<'_>,
    partial: &'a str,
) -> impl Iterator<Item = String> + 'a {
    let guild_id = match ctx.guild_id() {
        Some(id) => id.get() as i64,
        None => return Vec::new().into_iter(),
    };
    let dungeons = db::dungeon::list_for_guild(&ctx.data().db, guild_id)
        .await
        .unwrap_or_default();
    dungeons
        .into_iter()
        .filter(move |d| d.name.to_lowercase().contains(&partial.to_lowercase()))
        .map(|d| d.name)
        .collect::<Vec<_>>()
        .into_iter()
}

/// Manage tiers (isolated raid sections like Main, Veterans, Elite).
#[poise::command(
    slash_command,
    guild_only,
    subcommands("create", "delete", "list", "edit", "add_role", "remove_role", "add_dungeon", "remove_dungeon"),
    subcommand_required
)]
pub async fn tier(_ctx: BotContext<'_>) -> Result<(), BotError> {
    Ok(())
}

/// Create a new tier.
#[poise::command(slash_command, guild_only)]
pub async fn create(
    ctx: BotContext<'_>,
    #[description = "Tier name (e.g. Main, Veterans)"] name: String,
    #[description = "Optional description"] description: Option<String>,
) -> Result<(), BotError> {
    perm_svc::require(ctx, Action::ManageTiers, None, None).await?;

    let guild_id = ctx.guild_id().unwrap().get() as i64;

    match db::tier::create(&ctx.data().db, guild_id, &name, description.as_deref()).await {
        Ok(tier) => {
            ctx.say(format!("Created tier **{}** (id: {}).", tier.name, tier.id))
                .await?;
        }
        Err(e) if e.to_string().contains("unique") || e.to_string().contains("duplicate") => {
            ctx.say(format!(
                "A tier named `{name}` already exists in this server."
            ))
            .await?;
        }
        Err(e) => return Err(e.into()),
    }

    Ok(())
}

/// Delete a tier.
#[poise::command(slash_command, guild_only)]
pub async fn delete(
    ctx: BotContext<'_>,
    #[description = "Tier name"]
    #[autocomplete = "autocomplete_tier"]
    name: String,
) -> Result<(), BotError> {
    perm_svc::require(ctx, Action::ManageTiers, None, None).await?;

    let guild_id = ctx.guild_id().unwrap().get() as i64;
    let pool = &ctx.data().db;

    match db::tier::get_by_name(pool, guild_id, &name).await? {
        None => {
            ctx.say(format!("Tier `{name}` not found.")).await?;
        }
        Some(tier) => {
            db::tier::delete(pool, tier.id).await?;
            ctx.say(format!("Deleted tier **{name}**.")).await?;
        }
    }

    Ok(())
}

/// List all tiers in this server.
#[poise::command(slash_command, guild_only)]
pub async fn list(ctx: BotContext<'_>) -> Result<(), BotError> {
    perm_svc::require(ctx, Action::ManageTiers, None, None).await?;

    let guild_id = ctx.guild_id().unwrap().get() as i64;
    let tiers = db::tier::list(&ctx.data().db, guild_id).await?;

    if tiers.is_empty() {
        ctx.say("No tiers configured. Use `/tier create` to add one.")
            .await?;
        return Ok(());
    }

    let mut lines = vec!["**Tiers**".to_string()];
    for t in &tiers {
        let runs = t
            .runs_channel_id
            .map(|c| format!(" | Runs: <#{c}>"))
            .unwrap_or_default();
        let desc = t
            .description
            .as_deref()
            .map(|d| format!(" — {d}"))
            .unwrap_or_default();
        lines.push(format!("**{}** (id: {}){}{}", t.name, t.id, desc, runs));
    }

    ctx.say(lines.join("\n")).await?;
    Ok(())
}

/// Edit a tier's name, description, or runs channel.
#[poise::command(slash_command, guild_only)]
pub async fn edit(
    ctx: BotContext<'_>,
    #[description = "Tier to edit"]
    #[autocomplete = "autocomplete_tier"]
    name: String,
    #[description = "New name"] new_name: Option<String>,
    #[description = "New description"] description: Option<String>,
    #[description = "Runs channel (headcounts + runs post here)"]
    runs_channel: Option<serenity::GuildChannel>,
) -> Result<(), BotError> {
    perm_svc::require(ctx, Action::ManageTiers, None, None).await?;

    let guild_id = ctx.guild_id().unwrap().get() as i64;
    let pool = &ctx.data().db;

    let tier = match db::tier::get_by_name(pool, guild_id, &name).await? {
        Some(t) => t,
        None => {
            ctx.say(format!("Tier `{name}` not found.")).await?;
            return Ok(());
        }
    };

    if new_name.is_none() && description.is_none() && runs_channel.is_none() {
        ctx.say("Nothing to update — provide at least one field.")
            .await?;
        return Ok(());
    }

    let updated = db::tier::update(
        pool,
        tier.id,
        new_name.as_deref(),
        description.as_deref(),
        runs_channel.map(|c| c.id.get() as i64),
    )
    .await?;

    match updated {
        Some(t) => {
            ctx.say(format!("Updated tier **{}**.", t.name)).await?;
        }
        None => {
            ctx.say("Tier not found after update — this shouldn't happen.")
                .await?;
        }
    }

    Ok(())
}

/// Assign a Discord role as an access role for a tier.
#[poise::command(slash_command, guild_only, rename = "add-role")]
pub async fn add_role(
    ctx: BotContext<'_>,
    #[description = "Tier name"]
    #[autocomplete = "autocomplete_tier"]
    tier: String,
    #[description = "Role to assign"] role: serenity::Role,
) -> Result<(), BotError> {
    perm_svc::require(ctx, Action::ManageTiers, None, None).await?;

    let guild_id = ctx.guild_id().unwrap().get() as i64;
    let pool = &ctx.data().db;

    let t = match db::tier::get_by_name(pool, guild_id, &tier).await? {
        Some(t) => t,
        None => {
            ctx.say(format!("Tier `{tier}` not found.")).await?;
            return Ok(());
        }
    };

    let added = db::tier::add_role(pool, t.id, role.id.get() as i64).await?;
    if added {
        ctx.say(format!("Added <@&{}> as an access role for **{}**.", role.id, t.name))
            .await?;
    } else {
        ctx.say("That role is already assigned to this tier.")
            .await?;
    }

    Ok(())
}

/// Remove a Discord role from a tier's access roles.
#[poise::command(slash_command, guild_only, rename = "remove-role")]
pub async fn remove_role(
    ctx: BotContext<'_>,
    #[description = "Tier name"]
    #[autocomplete = "autocomplete_tier"]
    tier: String,
    #[description = "Role to remove"] role: serenity::Role,
) -> Result<(), BotError> {
    perm_svc::require(ctx, Action::ManageTiers, None, None).await?;

    let guild_id = ctx.guild_id().unwrap().get() as i64;
    let pool = &ctx.data().db;

    let t = match db::tier::get_by_name(pool, guild_id, &tier).await? {
        Some(t) => t,
        None => {
            ctx.say(format!("Tier `{tier}` not found.")).await?;
            return Ok(());
        }
    };

    let removed = db::tier::remove_role(pool, t.id, role.id.get() as i64).await?;
    if removed {
        ctx.say(format!("Removed <@&{}> from **{}**.", role.id, t.name))
            .await?;
    } else {
        ctx.say("That role was not assigned to this tier.").await?;
    }

    Ok(())
}

/// Add a dungeon to the set available in a tier.
#[poise::command(slash_command, guild_only, rename = "add-dungeon")]
pub async fn add_dungeon(
    ctx: BotContext<'_>,
    #[description = "Tier name"]
    #[autocomplete = "autocomplete_tier"]
    tier: String,
    #[description = "Dungeon name"]
    #[autocomplete = "autocomplete_dungeon"]
    dungeon: String,
) -> Result<(), BotError> {
    perm_svc::require(ctx, Action::ManageTiers, None, None).await?;

    let guild_id = ctx.guild_id().unwrap().get() as i64;
    let pool = &ctx.data().db;

    let t = match db::tier::get_by_name(pool, guild_id, &tier).await? {
        Some(t) => t,
        None => {
            ctx.say(format!("Tier `{tier}` not found.")).await?;
            return Ok(());
        }
    };

    let d = match db::dungeon::get_by_name(pool, guild_id, &dungeon).await? {
        Some(d) => d,
        None => {
            ctx.say(format!("Dungeon `{dungeon}` not found.")).await?;
            return Ok(());
        }
    };

    let added = db::tier::add_dungeon(pool, t.id, d.id).await?;
    if added {
        ctx.say(format!("Added **{}** to tier **{}**.", d.display_name, t.name))
            .await?;
    } else {
        ctx.say("That dungeon is already in this tier.").await?;
    }

    Ok(())
}

/// Remove a dungeon from a tier.
#[poise::command(slash_command, guild_only, rename = "remove-dungeon")]
pub async fn remove_dungeon(
    ctx: BotContext<'_>,
    #[description = "Tier name"]
    #[autocomplete = "autocomplete_tier"]
    tier: String,
    #[description = "Dungeon name"]
    #[autocomplete = "autocomplete_dungeon"]
    dungeon: String,
) -> Result<(), BotError> {
    perm_svc::require(ctx, Action::ManageTiers, None, None).await?;

    let guild_id = ctx.guild_id().unwrap().get() as i64;
    let pool = &ctx.data().db;

    let t = match db::tier::get_by_name(pool, guild_id, &tier).await? {
        Some(t) => t,
        None => {
            ctx.say(format!("Tier `{tier}` not found.")).await?;
            return Ok(());
        }
    };

    let d = match db::dungeon::get_by_name(pool, guild_id, &dungeon).await? {
        Some(d) => d,
        None => {
            ctx.say(format!("Dungeon `{dungeon}` not found.")).await?;
            return Ok(());
        }
    };

    let removed = db::tier::remove_dungeon(pool, t.id, d.id).await?;
    if removed {
        ctx.say(format!(
            "Removed **{}** from tier **{}**.",
            d.display_name, t.name
        ))
        .await?;
    } else {
        ctx.say("That dungeon was not in this tier.").await?;
    }

    Ok(())
}
