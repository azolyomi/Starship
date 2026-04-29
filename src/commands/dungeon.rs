use poise::serenity_prelude as serenity;
use serenity::CreateInteractionResponse;

use crate::db::dungeon as db;
use crate::handlers::dungeon_edit;
use crate::{guild_id_i64, limits, BotContext, BotError};

/// Manage dungeon templates for this server.
#[poise::command(
    slash_command,
    guild_only,
    subcommands("list", "create", "edit", "delete"),
    default_member_permissions = "MANAGE_GUILD"
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
    let guild_id = guild_id_i64(ctx);
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

async fn autocomplete_inherit<'a>(
    ctx: BotContext<'_>,
    partial: &'a str,
) -> impl Iterator<Item = serenity::AutocompleteChoice> + 'a {
    let guild_id = match ctx.guild_id() {
        Some(id) => id.get() as i64,
        None => return Vec::new().into_iter(),
    };
    let needle = partial.to_lowercase();
    db::list_for_guild(&ctx.data().db, guild_id)
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

/// Create a custom dungeon template for this server.
///
/// Opens an interactive modal for the display name + description + color,
/// then a follow-up ephemeral with controls for reactions, tiers, and
/// the requires-VC flag. The optional `inherit` arg autofills the new
/// template's scalars and reactions from another dungeon — handy for
/// "I want my own version of Lost Halls with one extra reaction".
#[poise::command(slash_command, guild_only)]
pub async fn create(
    ctx: BotContext<'_>,
    #[description = "Existing dungeon to autofill from (optional)"]
    #[autocomplete = "autocomplete_inherit"]
    inherit: Option<String>,
) -> Result<(), BotError> {
    let guild_id = guild_id_i64(ctx);
    let pool = &ctx.data().db;

    // Cap check before opening the modal so a fully-loaded server gets a
    // clear rejection instead of typing into a form for no reason.
    let count = db::count_guild_templates(pool, guild_id).await?;
    if count >= limits::CUSTOM_DUNGEONS_PER_GUILD {
        ctx.send(
            poise::CreateReply::default()
                .content(format!(
                    "This server is at the cap of {} custom dungeons. \
                     Delete one with `/dungeon delete` before creating another.",
                    limits::CUSTOM_DUNGEONS_PER_GUILD
                ))
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    }

    let inherit_template = match inherit.as_deref() {
        Some(name) => db::get_by_name(pool, guild_id, name).await?,
        None => None,
    };

    // Modals can only be sent in response to a non-deferred interaction.
    // Reach into the application context to get the raw command
    // interaction; poise has no helper for "respond with modal".
    let app_ctx = match ctx {
        poise::Context::Application(app) => app,
        poise::Context::Prefix(_) => {
            ctx.say("/dungeon create can only be used as a slash command.")
                .await?;
            return Ok(());
        }
    };

    app_ctx
        .interaction
        .create_response(
            ctx.http(),
            CreateInteractionResponse::Modal(dungeon_edit::build_create_modal(
                inherit_template.as_ref(),
            )),
        )
        .await?;
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
    let guild_id = guild_id_i64(ctx);
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
    let guild_id = guild_id_i64(ctx);
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
