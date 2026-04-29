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

async fn autocomplete_edit_target<'a>(
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

/// Edit a dungeon template's reactions, tier attachment, and text fields.
///
/// Globals are seed-managed (overrides.json) and shouldn't mutate
/// per-guild, so editing one auto-forks it into a guild-specific copy
/// first; a one-line note in the ephemeral surfaces the fork. The
/// ephemeral is the same one `/dungeon create` opens after its modal.
#[poise::command(slash_command, guild_only)]
pub async fn edit(
    ctx: BotContext<'_>,
    #[description = "Dungeon to edit"]
    #[autocomplete = "autocomplete_edit_target"]
    name: String,
) -> Result<(), BotError> {
    let guild_id = guild_id_i64(ctx);
    let pool = &ctx.data().db;

    let Some(target) = db::get_by_name(pool, guild_id, &name).await? else {
        ctx.send(
            poise::CreateReply::default()
                .content(format!(
                    "No dungeon named `{name}` found. Try the autocomplete list."
                ))
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    };

    let (template_id, intro_note) = if target.guild_id.is_none() {
        // Global: fork before letting the user mutate it.
        let count = db::count_guild_templates(pool, guild_id).await?;
        if count >= limits::CUSTOM_DUNGEONS_PER_GUILD {
            ctx.send(
                poise::CreateReply::default()
                    .content(format!(
                        "Editing a global forks it into a guild-specific copy, but this server is \
                         already at the {}-dungeon cap. Delete an unused custom dungeon with \
                         `/dungeon delete` first.",
                        limits::CUSTOM_DUNGEONS_PER_GUILD
                    ))
                    .ephemeral(true),
            )
            .await?;
            return Ok(());
        }
        let new_id = db::clone_global_to_guild(pool, target.id, guild_id).await?;
        (
            new_id,
            Some(format!(
                "Forked global `{}` into a guild-specific copy. Edits below apply only to this \
                 server's copy; the global stays untouched.",
                target.name
            )),
        )
    } else {
        (target.id, None)
    };

    let app_ctx = match ctx {
        poise::Context::Application(app) => app,
        poise::Context::Prefix(_) => {
            ctx.say("/dungeon edit can only be used as a slash command.")
                .await?;
            return Ok(());
        }
    };
    let response =
        dungeon_edit::build_edit_response(pool, template_id, intro_note.as_deref()).await?;
    app_ctx
        .interaction
        .create_response(ctx.http(), CreateInteractionResponse::Message(response))
        .await?;
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
