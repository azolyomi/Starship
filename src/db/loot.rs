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

/// Returns the stored threshold tier name for `(guild_id, dungeon_template_id)`,
/// defaulting to `white` (strictest) when no row exists.
pub async fn get_threshold(
    pool: &PgPool,
    guild_id: i64,
    dungeon_template_id: i32,
) -> Result<String> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT tier_name FROM guild_loot_tier_threshold
         WHERE guild_id = $1 AND dungeon_template_id = $2",
    )
    .bind(guild_id)
    .bind(dungeon_template_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(t,)| t).unwrap_or_else(|| "white".to_string()))
}

/// Upsert a per-dungeon bag-tier threshold for a guild. `tier_name` must be
/// a row in `bag_tiers`; the FK enforces that at the DB level.
pub async fn set_threshold(
    pool: &PgPool,
    guild_id: i64,
    dungeon_template_id: i32,
    tier_name: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO guild_loot_tier_threshold (guild_id, dungeon_template_id, tier_name)
         VALUES ($1, $2, $3)
         ON CONFLICT (guild_id, dungeon_template_id)
         DO UPDATE SET tier_name = EXCLUDED.tier_name, updated_at = NOW()",
    )
    .bind(guild_id)
    .bind(dungeon_template_id)
    .bind(tier_name)
    .execute(pool)
    .await?;
    Ok(())
}
