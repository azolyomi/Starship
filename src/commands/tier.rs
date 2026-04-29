use poise::serenity_prelude as serenity;

use crate::{
    db, guild_id_i64,
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
) -> impl Iterator<Item = serenity::AutocompleteChoice> + 'a {
    let guild_id = match ctx.guild_id() {
        Some(id) => id.get() as i64,
        None => return Vec::new().into_iter(),
    };
    let needle = partial.to_lowercase();
    db::dungeon::list_for_guild(&ctx.data().db, guild_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(move |d| {
            d.display_name.to_lowercase().contains(&needle)
                || d.name.to_lowercase().contains(&needle)
        })
        .map(|d| serenity::AutocompleteChoice::new(d.display_name, d.name))
        .collect::<Vec<_>>()
        .into_iter()
}

/// Manage tiers (isolated raid sections like Main, Veterans, Elite).
#[poise::command(
    slash_command,
    guild_only,
    subcommands(
        "create",
        "delete",
        "list",
        "edit",
        "add_leader",
        "remove_leader",
        "add_dungeon",
        "remove_dungeon"
    ),
    subcommand_required,
    default_member_permissions = "MANAGE_GUILD"
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

    let guild_id = guild_id_i64(ctx);

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

    let guild_id = guild_id_i64(ctx);
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

    let guild_id = guild_id_i64(ctx);
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
    #[description = "Runs channel (headcounts + runs post here)"] runs_channel: Option<
        serenity::GuildChannel,
    >,
) -> Result<(), BotError> {
    perm_svc::require(ctx, Action::ManageTiers, None, None).await?;

    let guild_id = guild_id_i64(ctx);
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

/// Grant a role permission to lead raids in this tier.
// Writes the full `LEADER_ACTIONS` set (StartHeadcount, StartRun, EndRun, …)
// scoped to this tier only — guild-wide grants are untouched.
#[poise::command(slash_command, guild_only, rename = "add-leader")]
pub async fn add_leader(
    ctx: BotContext<'_>,
    #[description = "Tier name"]
    #[autocomplete = "autocomplete_tier"]
    tier: String,
    #[description = "Role to grant"] role: serenity::Role,
) -> Result<(), BotError> {
    perm_svc::require(ctx, Action::ManageTiers, None, None).await?;

    let guild_id = guild_id_i64(ctx);
    let pool = &ctx.data().db;

    let t = match db::tier::get_by_name(pool, guild_id, &tier).await? {
        Some(t) => t,
        None => {
            ctx.say(format!("Tier `{tier}` not found.")).await?;
            return Ok(());
        }
    };

    let role_id = role.id.get() as i64;
    let mut any_inserted = false;
    for action in perm_svc::LEADER_ACTIONS {
        if db::permission::grant(pool, guild_id, role_id, action, Some(t.id), None).await? {
            any_inserted = true;
        }
    }

    if any_inserted {
        ctx.say(format!(
            "Added <@&{}> as a leader role for **{}**.",
            role.id, t.name
        ))
        .await?;
    } else {
        ctx.say("That role already leads this tier.").await?;
    }

    Ok(())
}

/// Revoke a role's permission to lead raids in this tier.
// Removes the full `LEADER_ACTIONS` set scoped to this tier; guild-wide
// grants (e.g. from the express-setup Raid Leader role) are untouched.
#[poise::command(slash_command, guild_only, rename = "remove-leader")]
pub async fn remove_leader(
    ctx: BotContext<'_>,
    #[description = "Tier name"]
    #[autocomplete = "autocomplete_tier"]
    tier: String,
    #[description = "Role to revoke"] role: serenity::Role,
) -> Result<(), BotError> {
    perm_svc::require(ctx, Action::ManageTiers, None, None).await?;

    let guild_id = guild_id_i64(ctx);
    let pool = &ctx.data().db;

    let t = match db::tier::get_by_name(pool, guild_id, &tier).await? {
        Some(t) => t,
        None => {
            ctx.say(format!("Tier `{tier}` not found.")).await?;
            return Ok(());
        }
    };

    let role_id = role.id.get() as i64;
    let mut any_removed = false;
    for action in perm_svc::LEADER_ACTIONS {
        if db::permission::revoke(pool, guild_id, role_id, action, Some(t.id), None).await? {
            any_removed = true;
        }
    }

    if any_removed {
        ctx.say(format!(
            "Removed <@&{}> as a leader of **{}**.",
            role.id, t.name
        ))
        .await?;
    } else {
        ctx.say("That role wasn't a leader of this tier.").await?;
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

    let guild_id = guild_id_i64(ctx);
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

    let changed = db::tier::add_dungeon(pool, t.id, &d).await?;
    let is_global = d.guild_id.is_none();
    let msg = match (changed, is_global) {
        (true, true) => format!(
            "Re-enabled **{}** for tier **{}**.",
            d.display_name, t.name
        ),
        (true, false) => format!(
            "Added **{}** to tier **{}**.",
            d.display_name, t.name
        ),
        (false, true) => format!(
            "**{}** is already enabled for tier **{}**.",
            d.display_name, t.name
        ),
        (false, false) => format!(
            "**{}** is already attached to tier **{}**.",
            d.display_name, t.name
        ),
    };
    ctx.say(msg).await?;

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

    let guild_id = guild_id_i64(ctx);
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

    let changed = db::tier::remove_dungeon(pool, t.id, &d).await?;
    let is_global = d.guild_id.is_none();
    let msg = match (changed, is_global) {
        (true, true) => format!(
            "Disabled **{}** for tier **{}**. Re-enable any time with \
             `/tier add-dungeon`.",
            d.display_name, t.name
        ),
        (true, false) => format!(
            "Removed **{}** from tier **{}**.",
            d.display_name, t.name
        ),
        (false, true) => format!(
            "**{}** is already disabled for tier **{}**.",
            d.display_name, t.name
        ),
        (false, false) => format!(
            "**{}** is not attached to tier **{}**.",
            d.display_name, t.name
        ),
    };
    ctx.say(msg).await?;

    Ok(())
}
