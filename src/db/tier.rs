use anyhow::Result;
use sqlx::PgPool;

use crate::db::models::Tier;

// The `tiers.*` projection used by every read here. Kept as one constant
// so adding a new column doesn't require touching five queries.
const TIER_COLS: &str = "id, guild_id, name, description, runs_channel_id, \
    enable_self_organization, self_organize_channel_id, \
    self_organize_button_message_id, self_organize_listing_message_id, \
    self_organize_idle_minutes, self_organize_cancel_cooldown_seconds, \
    self_organize_min_reactors, created_at";

pub async fn create(
    pool: &PgPool,
    guild_id: i64,
    name: &str,
    description: Option<&str>,
) -> Result<Tier> {
    let tier = sqlx::query_as::<_, Tier>(&format!(
        "INSERT INTO tiers (guild_id, name, description)
         VALUES ($1, $2, $3)
         RETURNING {TIER_COLS}"
    ))
    .bind(guild_id)
    .bind(name)
    .bind(description)
    .fetch_one(pool)
    .await?;
    Ok(tier)
}

pub async fn list(pool: &PgPool, guild_id: i64) -> Result<Vec<Tier>> {
    let tiers = sqlx::query_as::<_, Tier>(&format!(
        "SELECT {TIER_COLS} FROM tiers WHERE guild_id = $1 ORDER BY id"
    ))
    .bind(guild_id)
    .fetch_all(pool)
    .await?;
    Ok(tiers)
}

/// Every tier across every guild with `enable_self_organization = TRUE`.
/// Used by the boot orphan sweep to repair sticky button + listing
/// messages and reconcile dangling slot claims.
pub async fn list_self_organize_enabled(pool: &PgPool) -> Result<Vec<Tier>> {
    let tiers = sqlx::query_as::<_, Tier>(&format!(
        "SELECT {TIER_COLS} FROM tiers WHERE enable_self_organization = TRUE"
    ))
    .fetch_all(pool)
    .await?;
    Ok(tiers)
}

pub async fn get_by_id(pool: &PgPool, id: i32) -> Result<Option<Tier>> {
    let tier = sqlx::query_as::<_, Tier>(&format!("SELECT {TIER_COLS} FROM tiers WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(tier)
}

pub async fn get_by_name(pool: &PgPool, guild_id: i64, name: &str) -> Result<Option<Tier>> {
    let tier = sqlx::query_as::<_, Tier>(&format!(
        "SELECT {TIER_COLS} FROM tiers WHERE guild_id = $1 AND name = $2"
    ))
    .bind(guild_id)
    .bind(name)
    .fetch_optional(pool)
    .await?;
    Ok(tier)
}

pub async fn update(
    pool: &PgPool,
    id: i32,
    name: Option<&str>,
    description: Option<&str>,
    runs_channel_id: Option<i64>,
) -> Result<Option<Tier>> {
    let tier = sqlx::query_as::<_, Tier>(&format!(
        "UPDATE tiers
         SET name            = COALESCE($2, name),
             description     = COALESCE($3, description),
             runs_channel_id = COALESCE($4, runs_channel_id)
         WHERE id = $1
         RETURNING {TIER_COLS}"
    ))
    .bind(id)
    .bind(name)
    .bind(description)
    .bind(runs_channel_id)
    .fetch_optional(pool)
    .await?;
    Ok(tier)
}

/// Update self-organize knobs on a tier. Each Option is a partial update
/// (NULL = leave alone). Used by the `/setup` self-organize sub-step.
#[allow(clippy::too_many_arguments)]
pub async fn update_self_organize(
    pool: &PgPool,
    id: i32,
    enabled: Option<bool>,
    channel_id: Option<i64>,
    idle_minutes: Option<i32>,
    cancel_cooldown_seconds: Option<i32>,
    min_reactors: Option<i32>,
) -> Result<Option<Tier>> {
    let tier = sqlx::query_as::<_, Tier>(&format!(
        "UPDATE tiers
         SET enable_self_organization              = COALESCE($2, enable_self_organization),
             self_organize_channel_id              = COALESCE($3, self_organize_channel_id),
             self_organize_idle_minutes            = COALESCE($4, self_organize_idle_minutes),
             self_organize_cancel_cooldown_seconds = COALESCE($5, self_organize_cancel_cooldown_seconds),
             self_organize_min_reactors            = COALESCE($6, self_organize_min_reactors)
         WHERE id = $1
         RETURNING {TIER_COLS}"
    ))
    .bind(id)
    .bind(enabled)
    .bind(channel_id)
    .bind(idle_minutes)
    .bind(cancel_cooldown_seconds)
    .bind(min_reactors)
    .fetch_optional(pool)
    .await?;
    Ok(tier)
}

/// Direct setter for the sticky button message ID — bypasses
/// `update_self_organize` because sticky-repair runs on every config save
/// and a full COALESCE update is wasteful for a single-column write.
pub async fn set_self_organize_button_message(
    pool: &PgPool,
    id: i32,
    message_id: Option<i64>,
) -> Result<()> {
    sqlx::query!(
        "UPDATE tiers SET self_organize_button_message_id = $1 WHERE id = $2",
        message_id,
        id
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Direct setter for the sticky listing message ID. See
/// `set_self_organize_button_message` for the rationale.
pub async fn set_self_organize_listing_message(
    pool: &PgPool,
    id: i32,
    message_id: Option<i64>,
) -> Result<()> {
    sqlx::query!(
        "UPDATE tiers SET self_organize_listing_message_id = $1 WHERE id = $2",
        message_id,
        id
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete(pool: &PgPool, id: i32) -> Result<bool> {
    let result = sqlx::query!("DELETE FROM tiers WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn add_dungeon(pool: &PgPool, tier_id: i32, dungeon_template_id: i32) -> Result<bool> {
    let result = sqlx::query!(
        "INSERT INTO tier_dungeons (tier_id, dungeon_template_id)
         VALUES ($1, $2) ON CONFLICT DO NOTHING",
        tier_id,
        dungeon_template_id
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn remove_dungeon(pool: &PgPool, tier_id: i32, dungeon_template_id: i32) -> Result<bool> {
    let result = sqlx::query!(
        "DELETE FROM tier_dungeons WHERE tier_id = $1 AND dungeon_template_id = $2",
        tier_id,
        dungeon_template_id
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn list_dungeons(pool: &PgPool, tier_id: i32) -> Result<Vec<i32>> {
    let ids = sqlx::query_scalar!(
        "SELECT dungeon_template_id FROM tier_dungeons WHERE tier_id = $1 ORDER BY dungeon_template_id",
        tier_id
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}
