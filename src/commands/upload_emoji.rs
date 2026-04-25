//! `/upload-emoji` — operator-only. Discord-side counterpart to the
//! `starship upload-emoji` CLI. Same job (push a PNG to the bot's
//! application emoji set and register it in `bot_emoji`), but reachable
//! from inside Discord so the operator doesn't need shell access to the
//! VPS to fill in a single missing icon.
//!
//! Gated strictly to `services::permission::GLOBAL_SUPERADMIN_USER_ID`.
//! Not even guild superadmins or `ManageServer` holders can run it —
//! the bot's application-emoji set is shared across every guild it's
//! in, so this is operator surface, not server-admin surface.

use poise::serenity_prelude as serenity;
use reqwest::Client;

use crate::db;
use crate::db::emoji::ApplicationEmojiClient;
use crate::services::permission::GLOBAL_SUPERADMIN_USER_ID;
use crate::{BotContext, BotError};

/// Operator-only: upload a PNG as an application emoji (for scraper misses).
#[poise::command(slash_command, rename = "upload-emoji")]
pub async fn upload_emoji(
    ctx: BotContext<'_>,
    #[description = "Logical name used in code (e.g. wine_cellar_incantation)"] name: String,
    #[description = "PNG file (≤256KB, ideally 128×128)"] image: serenity::Attachment,
    #[description = "Override Discord-side name (≤32 chars, alphanumeric+underscore)"]
    discord_name: Option<String>,
    #[description = "Category label: ui, key, drop, drop_shiny"] category: Option<String>,
    #[description = "Bag tier (white, cyan, etc.) — only for drop emojis"] bag_tier: Option<String>,
) -> Result<(), BotError> {
    if ctx.author().id.get() != GLOBAL_SUPERADMIN_USER_ID {
        ctx.send(
            poise::CreateReply::default()
                .content("Not authorized.")
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    }

    ctx.defer_ephemeral().await?;

    // Basic shape checks before spending a Discord round-trip.
    if image.size > 256 * 1024 {
        reply_err(
            ctx,
            format!(
                "Attachment is {}KB — Discord's app-emoji limit is 256KB.",
                image.size / 1024
            ),
        )
        .await?;
        return Ok(());
    }
    let bytes = image.download().await?;
    if !bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        reply_err(ctx, "Attachment isn't a PNG (magic bytes don't match).").await?;
        return Ok(());
    }

    let name_on_discord = discord_name.unwrap_or_else(|| discord_safe_name(&name));
    if name_on_discord.is_empty() {
        reply_err(ctx, "Derived Discord-side name is empty after sanitising.").await?;
        return Ok(());
    }
    if name_on_discord.len() > 32 {
        reply_err(
            ctx,
            format!(
                "Discord-side name `{name_on_discord}` is {} chars; the limit is 32. \
             Pass a shorter value with the `discord_name` parameter.",
                name_on_discord.len()
            ),
        )
        .await?;
        return Ok(());
    }

    let config = &ctx.data().config;
    let client = ApplicationEmojiClient::new(
        Client::new(),
        &config.discord_token,
        config.discord_application_id,
    );

    // Idempotent on re-runs: reuse an existing app emoji with the same
    // Discord-side name rather than creating a duplicate.
    let existing = client.list().await?;
    let (emoji_id, animated) = match existing.get(&name_on_discord) {
        Some(&(id, animated)) => (id, animated),
        None => client.create(&name_on_discord, &bytes).await?,
    };

    db::emoji::upsert(
        &ctx.data().db,
        &name,
        emoji_id as i64,
        &name_on_discord,
        animated,
        None,
        category.as_deref(),
        None,
        bag_tier.as_deref(),
    )
    .await?;

    let rendered = if animated {
        format!("<a:{name_on_discord}:{emoji_id}>")
    } else {
        format!("<:{name_on_discord}:{emoji_id}>")
    };
    ctx.send(
        poise::CreateReply::default()
            .content(format!(
                "✔ Uploaded `{name}` as {rendered} (Discord name: `{name_on_discord}`, id: `{emoji_id}`)."
            ))
            .ephemeral(true),
    )
    .await?;

    Ok(())
}

async fn reply_err(ctx: BotContext<'_>, msg: impl Into<String>) -> Result<(), BotError> {
    ctx.send(
        poise::CreateReply::default()
            .content(format!("⚠ {}", msg.into()))
            .ephemeral(true),
    )
    .await?;
    Ok(())
}

/// Coerce a logical name into a Discord-safe emoji name: alphanumeric +
/// underscore only, lowercased, capped at 32 chars. Mirrors the CLI's
/// sanitiser so the two code paths produce the same Discord-side name.
fn discord_safe_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(32));
    for c in s.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        }
        if out.len() == 32 {
            break;
        }
    }
    out
}
