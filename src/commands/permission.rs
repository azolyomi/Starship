use poise::serenity_prelude as serenity;

use crate::{
    db,
    services::permission::{self as perm_svc, Action, ALL_ACTIONS},
    BotContext, BotError,
};

async fn autocomplete_action<'a>(
    _ctx: BotContext<'_>,
    partial: &'a str,
) -> impl Iterator<Item = String> + 'a {
    ALL_ACTIONS
        .iter()
        .filter(move |a| a.to_lowercase().contains(&partial.to_lowercase()))
        .map(|a| a.to_string())
}

/// Manage permissions for roles in this server.
#[poise::command(
    slash_command,
    guild_only,
    subcommands("grant", "revoke", "list"),
    subcommand_required
)]
pub async fn permission(_ctx: BotContext<'_>) -> Result<(), BotError> {
    Ok(())
}

/// Grant a permission action to a role.
#[poise::command(slash_command, guild_only)]
pub async fn grant(
    ctx: BotContext<'_>,
    #[description = "Role to grant"] role: serenity::Role,
    #[description = "Action to grant"]
    #[autocomplete = "autocomplete_action"]
    action: String,
    #[description = "Scope to a specific tier (optional)"] tier: Option<String>,
    #[description = "Scope to a specific dungeon (optional)"] dungeon: Option<String>,
) -> Result<(), BotError> {
    perm_svc::require(ctx, Action::ManagePermissions, None, None).await?;

    let guild_id = ctx.guild_id().unwrap().get() as i64;
    let pool = &ctx.data().db;

    if !perm_svc::is_valid_action(&action) {
        ctx.say(format!(
            "Unknown action `{action}`. Valid actions:\n{}",
            ALL_ACTIONS.join(", ")
        ))
        .await?;
        return Ok(());
    }

    let tier_id = match &tier {
        Some(name) => match db::tier::get_by_name(pool, guild_id, name).await? {
            Some(t) => Some(t.id),
            None => {
                ctx.say(format!("Tier `{name}` not found.")).await?;
                return Ok(());
            }
        },
        None => None,
    };

    let dungeon_template_id = match &dungeon {
        Some(name) => match db::dungeon::get_by_name(pool, guild_id, name).await? {
            Some(d) => Some(d.id),
            None => {
                ctx.say(format!("Dungeon `{name}` not found.")).await?;
                return Ok(());
            }
        },
        None => None,
    };

    let inserted = db::permission::grant(
        pool,
        guild_id,
        role.id.get() as i64,
        &action,
        tier_id,
        dungeon_template_id,
    )
    .await?;

    if inserted {
        ctx.say(format!(
            "Granted `{action}` to <@&{}>{}{}.",
            role.id,
            tier.as_deref()
                .map(|t| format!(" (tier: {t})"))
                .unwrap_or_default(),
            dungeon
                .as_deref()
                .map(|d| format!(" (dungeon: {d})"))
                .unwrap_or_default(),
        ))
        .await?;
    } else {
        ctx.say("That permission already exists.").await?;
    }

    Ok(())
}

/// Revoke a permission action from a role.
#[poise::command(slash_command, guild_only)]
pub async fn revoke(
    ctx: BotContext<'_>,
    #[description = "Role to revoke from"] role: serenity::Role,
    #[description = "Action to revoke"]
    #[autocomplete = "autocomplete_action"]
    action: String,
    #[description = "Tier scope to revoke (optional)"] tier: Option<String>,
    #[description = "Dungeon scope to revoke (optional)"] dungeon: Option<String>,
) -> Result<(), BotError> {
    perm_svc::require(ctx, Action::ManagePermissions, None, None).await?;

    let guild_id = ctx.guild_id().unwrap().get() as i64;
    let pool = &ctx.data().db;

    if !perm_svc::is_valid_action(&action) {
        ctx.say(format!("Unknown action `{action}`.")).await?;
        return Ok(());
    }

    let tier_id = match &tier {
        Some(name) => match db::tier::get_by_name(pool, guild_id, name).await? {
            Some(t) => Some(t.id),
            None => {
                ctx.say(format!("Tier `{name}` not found.")).await?;
                return Ok(());
            }
        },
        None => None,
    };

    let dungeon_template_id = match &dungeon {
        Some(name) => match db::dungeon::get_by_name(pool, guild_id, name).await? {
            Some(d) => Some(d.id),
            None => {
                ctx.say(format!("Dungeon `{name}` not found.")).await?;
                return Ok(());
            }
        },
        None => None,
    };

    let removed = db::permission::revoke(
        pool,
        guild_id,
        role.id.get() as i64,
        &action,
        tier_id,
        dungeon_template_id,
    )
    .await?;

    if removed {
        ctx.say(format!("Revoked `{action}` from <@&{}>.", role.id))
            .await?;
    } else {
        ctx.say("No matching permission found.").await?;
    }

    Ok(())
}

/// List all permissions configured for this server.
#[poise::command(slash_command, guild_only)]
pub async fn list(ctx: BotContext<'_>) -> Result<(), BotError> {
    perm_svc::require(ctx, Action::ManagePermissions, None, None).await?;

    let guild_id = ctx.guild_id().unwrap().get() as i64;
    let rows = db::permission::list_for_guild(&ctx.data().db, guild_id).await?;

    if rows.is_empty() {
        ctx.say("No permissions configured for this server.").await?;
        return Ok(());
    }

    let lines: Vec<String> = rows
        .iter()
        .map(|p| {
            let scope = match (p.tier_id, p.dungeon_template_id) {
                (None, None) => String::new(),
                (Some(t), None) => format!(" [tier:{t}]"),
                (None, Some(d)) => format!(" [dungeon:{d}]"),
                (Some(t), Some(d)) => format!(" [tier:{t}, dungeon:{d}]"),
            };
            format!("<@&{}> → `{}`{}", p.role_id, p.action, scope)
        })
        .collect();

    let body = lines.join("\n");
    // Discord message limit is 2000 chars; truncate gracefully if needed.
    if body.len() > 1900 {
        ctx.say(format!("{}\n…(truncated)", &body[..1900])).await?;
    } else {
        ctx.say(body).await?;
    }

    Ok(())
}
