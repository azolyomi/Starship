//! One-off manual application-emoji upload.
//!
//! The RealmEye scraper misses the occasional item — either because the wiki
//! doesn't have a canonical image for it yet (e.g. `wine_cellar_incantation`
//! before the R4 O3 template landed), or because the sprite lives somewhere
//! the scraper doesn't look. This CLI lets the operator push a PNG file
//! into Discord's application-emoji set and register the mapping in
//! `bot_emoji` without re-running the whole `sync-wiki` pass.
//!
//! Example:
//!     starship upload-emoji \
//!         --name wine_cellar_incantation \
//!         --file ./art/wine_cellar_incantation.png

use anyhow::{Context, Result};
use reqwest::Client;
use tracing::info;

use crate::config::Config;
use crate::db;
use crate::db::emoji::ApplicationEmojiClient;

pub async fn run(
    logical_name: String,
    file: std::path::PathBuf,
    discord_name: Option<String>,
    category: Option<String>,
    bag_tier: Option<String>,
) -> Result<()> {
    let config = Config::from_env()?;
    let pool = db::create_pool(&config.database_url).await?;

    // Sanity-check the file exists + is a PNG before hitting Discord.
    let bytes = std::fs::read(&file)
        .with_context(|| format!("reading {}", file.display()))?;
    if !bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        anyhow::bail!("{}: not a PNG (magic bytes don't match)", file.display());
    }

    // Discord's emoji name rules are tighter than our logical names (no `'`,
    // no spaces, max 32 chars). Default to a coerced form of the logical
    // name; the operator can override with --discord-name.
    let name_on_discord = discord_name.unwrap_or_else(|| discord_safe_name(&logical_name));
    if name_on_discord.len() > 32 {
        anyhow::bail!(
            "Discord-side name `{name_on_discord}` is >32 chars. \
             Pass a shorter value via --discord-name."
        );
    }

    let client = ApplicationEmojiClient::new(
        Client::new(),
        &config.discord_token,
        config.discord_application_id,
    );

    // Diff against existing app emojis so a re-run is idempotent.
    let existing = client.list().await?;
    let (emoji_id, animated) = match existing.get(&name_on_discord) {
        Some(&(id, animated)) => {
            info!(
                name_on_discord,
                id,
                "emoji already registered with Discord — reusing id"
            );
            (id, animated)
        }
        None => {
            let (id, animated) = client
                .create(&name_on_discord, &bytes)
                .await
                .with_context(|| format!("creating application emoji `{name_on_discord}`"))?;
            info!(name_on_discord, id, "uploaded new application emoji");
            (id, animated)
        }
    };

    db::emoji::upsert(
        &pool,
        &logical_name,
        emoji_id as i64,
        &name_on_discord,
        animated,
        None, // app emoji, not guild-hosted
        category.as_deref(),
        None, // no realmeye URL for manual uploads
        bag_tier.as_deref(),
    )
    .await?;

    info!(
        logical_name,
        name_on_discord, emoji_id, "bot_emoji row updated"
    );
    println!("✔ uploaded `{logical_name}` as <:{name_on_discord}:{emoji_id}>");
    Ok(())
}

/// Coerce a logical name into something Discord will accept as an emoji
/// name: alphanumeric + underscore only, lowercased, capped at 32 chars.
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
