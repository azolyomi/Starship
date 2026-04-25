use std::collections::HashMap;

use anyhow::Result;
use sqlx::PgPool;

use crate::db::models::{BagTier, BotEmoji};

/// All bag tiers ordered from lowest sort_order (brown) to highest (white).
pub async fn list_bag_tiers(pool: &PgPool) -> Result<Vec<BagTier>> {
    let rows = sqlx::query_as::<_, BagTier>(
        "SELECT name, sort_order, default_emoji FROM bag_tiers ORDER BY sort_order",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Render a bag tier's emoji for use in embed text. Prefers the uploaded
/// application emoji named `bag_<tier>` so the guild's threshold fields get
/// real RotMG bag sprites; falls back to the unicode literal stored in
/// `bag_tiers.default_emoji` when the scraper hasn't uploaded one yet.
pub fn resolve_bag_emoji(tier: &BagTier, emoji_map: &HashMap<String, BotEmoji>) -> String {
    let logical = format!("bag_{}", tier.name);
    match emoji_map.get(&logical) {
        Some(e) if e.animated => format!("<a:{}:{}>", e.name_on_discord, e.discord_emoji_id),
        Some(e) => format!("<:{}:{}>", e.name_on_discord, e.discord_emoji_id),
        None => tier.default_emoji.clone(),
    }
}

/// Returns the guild's loot-tier threshold, defaulting to `white` (strictest)
/// if the guild row is missing. The column itself has a NOT NULL DEFAULT,
/// so the fallback only kicks in for guilds that haven't been `upsert`'d yet.
pub async fn get_threshold(pool: &PgPool, guild_id: i64) -> Result<String> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT loot_tier_threshold FROM guilds WHERE guild_id = $1")
            .bind(guild_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(t,)| t).unwrap_or_else(|| "white".to_string()))
}

/// Update the guild-wide loot-tier threshold. `tier_name` must be a row
/// in `bag_tiers` — the FK on `guilds.loot_tier_threshold` enforces that.
pub async fn set_threshold(pool: &PgPool, guild_id: i64, tier_name: &str) -> Result<()> {
    sqlx::query(
        "UPDATE guilds SET loot_tier_threshold = $2, updated_at = NOW() WHERE guild_id = $1",
    )
    .bind(guild_id)
    .bind(tier_name)
    .execute(pool)
    .await?;
    Ok(())
}
