use poise::serenity_prelude as serenity;

use crate::db::dungeon as db;
use crate::{BotContext, BotError};

/// Manage dungeon templates for this server.
#[poise::command(
    slash_command,
    guild_only,
    subcommands("list", "create", "edit", "delete")
)]
pub async fn dungeon(_ctx: BotContext<'_>) -> Result<(), BotError> {
    Ok(())
}

/// List dungeon templates available to this server.
#[poise::command(slash_command, guild_only)]
pub async fn list(
    ctx: BotContext<'_>,
    #[description = "Show only global built-in templates"] global_only: Option<bool>,
) -> Result<(), BotError> {
    let guild_id = ctx.guild_id().unwrap().get() as i64;
    let pool = &ctx.data().db;

    let templates = if global_only.unwrap_or(false) {
        db::list_for_guild(pool, 0).await? // 0 matches no guild, returns only globals
    } else {
        db::list_for_guild(pool, guild_id).await?
    };

    if templates.is_empty() {
        ctx.say("No dungeon templates found. Run `starship sync-wiki` to populate defaults.")
            .await?;
        return Ok(());
    }

    let mut fields: Vec<(String, String, bool)> = Vec::new();
    for t in &templates {
        let scope = if t.guild_id.is_some() {
            "custom"
        } else {
            "global"
        };
        let vc = if t.requires_vc { " • VC raid" } else { "" };
        fields.push((
            format!("{} ({})", t.display_name, scope),
            format!("`{}`{}", t.name, vc),
            true,
        ));
    }

    // Discord embeds support up to 25 fields; paginate if needed.
    let chunks: Vec<_> = fields.chunks(25).collect();
    let page_count = chunks.len();
    for (i, chunk) in chunks.into_iter().enumerate() {
        let title = if page_count > 1 {
            format!("Dungeon Templates ({}/{})", i + 1, page_count)
        } else {
            "Dungeon Templates".to_string()
        };
        let embed = serenity::CreateEmbed::new()
            .title(title)
            .color(0x5865F2_u32)
            .fields(chunk.iter().cloned());
        ctx.send(poise::CreateReply::default().embed(embed)).await?;
    }

    Ok(())
}

/// Create a custom dungeon template for this server.
#[poise::command(slash_command, guild_only)]
pub async fn create(
    ctx: BotContext<'_>,
    #[description = "Internal name (snake_case, unique per server)"] name: String,
    #[description = "Display name shown in embeds"] display_name: String,
    #[description = "Logical emoji name (from bot_emoji table)"] emoji: Option<String>,
    #[description = "Embed color as hex, e.g. FF4500"] color: Option<String>,
    #[description = "Headcount embed description"] description: Option<String>,
    #[description = "Whether this is a voice-channel raid"] requires_vc: Option<bool>,
) -> Result<(), BotError> {
    let guild_id = ctx.guild_id().unwrap().get() as i64;
    let pool = &ctx.data().db;

    // Basic name validation: lowercase alphanumeric + underscore only.
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        ctx.say("Name must be lowercase alphanumeric with underscores only (e.g. `my_dungeon`).")
            .await?;
        return Ok(());
    }

    let color_int = match color.as_deref() {
        Some(s) => {
            let hex = s.trim_start_matches('#');
            match i64::from_str_radix(hex, 16) {
                Ok(v) if v <= 0xFFFFFF => Some(v as i32),
                _ => {
                    ctx.say("Color must be a valid hex color like `FF4500`.")
                        .await?;
                    return Ok(());
                }
            }
        }
        None => None,
    };

    let t = db::NewTemplate {
        name: &name,
        display_name: &display_name,
        emoji: emoji.as_deref(),
        color: color_int,
        message_title: Some(&display_name),
        message_description: description.as_deref(),
        requires_vc: requires_vc.unwrap_or(false),
    };

    match db::insert_guild_template(pool, guild_id, &t).await {
        Ok(_) => {
            ctx.say(format!(
                "Created dungeon template `{}` — use `/dungeon edit` to refine it.",
                name
            ))
            .await?;
        }
        Err(e) if e.to_string().contains("unique") || e.to_string().contains("duplicate") => {
            ctx.say(format!(
                "A template named `{}` already exists for this server.",
                name
            ))
            .await?;
        }
        Err(e) => return Err(e.into()),
    }

    Ok(())
}

/// Edit a custom dungeon template. Only server-specific templates can be edited here.
#[poise::command(slash_command, guild_only)]
pub async fn edit(
    ctx: BotContext<'_>,
    #[description = "Internal name of the template to edit"] name: String,
    #[description = "New display name"] display_name: Option<String>,
    #[description = "New logical emoji name"] emoji: Option<String>,
    #[description = "New embed color as hex, e.g. FF4500"] color: Option<String>,
    #[description = "New headcount description"] description: Option<String>,
    #[description = "Change VC raid requirement"] requires_vc: Option<bool>,
) -> Result<(), BotError> {
    let guild_id = ctx.guild_id().unwrap().get() as i64;
    let pool = &ctx.data().db;

    let color_int = match color.as_deref() {
        Some(s) => {
            let hex = s.trim_start_matches('#');
            match i64::from_str_radix(hex, 16) {
                Ok(v) if v <= 0xFFFFFF => Some(v as i32),
                _ => {
                    ctx.say("Color must be a valid hex color like `FF4500`.")
                        .await?;
                    return Ok(());
                }
            }
        }
        None => None,
    };

    let updated = db::update_guild_template(
        pool,
        guild_id,
        &name,
        display_name.as_deref(),
        emoji.as_deref(),
        color_int,
        None,
        description.as_deref(),
        requires_vc,
    )
    .await?;

    if updated {
        ctx.say(format!("Updated template `{}`.", name)).await?;
    } else {
        ctx.say(format!(
            "No server-specific template named `{}` found. You can only edit templates created with `/dungeon create`.",
            name
        ))
        .await?;
    }

    Ok(())
}

/// Delete a custom dungeon template (built-in global templates cannot be deleted).
#[poise::command(slash_command, guild_only)]
pub async fn delete(
    ctx: BotContext<'_>,
    #[description = "Internal name of the template to delete"] name: String,
) -> Result<(), BotError> {
    let guild_id = ctx.guild_id().unwrap().get() as i64;
    let pool = &ctx.data().db;

    let deleted = db::delete_guild_template(pool, guild_id, &name).await?;

    if deleted {
        ctx.say(format!("Deleted template `{}`.", name)).await?;
    } else {
        ctx.say(format!(
            "No server-specific template named `{}` found. Global built-in templates cannot be deleted.",
            name
        ))
        .await?;
    }

    Ok(())
}
