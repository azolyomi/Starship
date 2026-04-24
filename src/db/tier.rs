use anyhow::Result;
use sqlx::PgPool;

use crate::db::models::Tier;

pub async fn create(
    pool: &PgPool,
    guild_id: i64,
    name: &str,
    description: Option<&str>,
) -> Result<Tier> {
    let tier = sqlx::query_as!(
        Tier,
        "INSERT INTO tiers (guild_id, name, description)
         VALUES ($1, $2, $3)
         RETURNING id, guild_id, name, description, runs_channel_id, created_at",
        guild_id,
        name,
        description
    )
    .fetch_one(pool)
    .await?;
    Ok(tier)
}

pub async fn list(pool: &PgPool, guild_id: i64) -> Result<Vec<Tier>> {
    let tiers = sqlx::query_as!(
        Tier,
        "SELECT id, guild_id, name, description, runs_channel_id, created_at
         FROM tiers WHERE guild_id = $1 ORDER BY id",
        guild_id
    )
    .fetch_all(pool)
    .await?;
    Ok(tiers)
}

pub async fn get_by_id(pool: &PgPool, id: i32) -> Result<Option<Tier>> {
    let tier = sqlx::query_as!(
        Tier,
        "SELECT id, guild_id, name, description, runs_channel_id, created_at
         FROM tiers WHERE id = $1",
        id
    )
    .fetch_optional(pool)
    .await?;
    Ok(tier)
}

pub async fn get_by_name(pool: &PgPool, guild_id: i64, name: &str) -> Result<Option<Tier>> {
    let tier = sqlx::query_as!(
        Tier,
        "SELECT id, guild_id, name, description, runs_channel_id, created_at
         FROM tiers WHERE guild_id = $1 AND name = $2",
        guild_id,
        name
    )
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
    let tier = sqlx::query_as!(
        Tier,
        "UPDATE tiers
         SET name            = COALESCE($2, name),
             description     = COALESCE($3, description),
             runs_channel_id = COALESCE($4, runs_channel_id)
         WHERE id = $1
         RETURNING id, guild_id, name, description, runs_channel_id, created_at",
        id,
        name,
        description,
        runs_channel_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(tier)
}

pub async fn delete(pool: &PgPool, id: i32) -> Result<bool> {
    let result = sqlx::query!("DELETE FROM tiers WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn add_role(pool: &PgPool, tier_id: i32, role_id: i64) -> Result<bool> {
    let result = sqlx::query!(
        "INSERT INTO tier_roles (tier_id, role_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        tier_id,
        role_id
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn remove_role(pool: &PgPool, tier_id: i32, role_id: i64) -> Result<bool> {
    let result = sqlx::query!(
        "DELETE FROM tier_roles WHERE tier_id = $1 AND role_id = $2",
        tier_id,
        role_id
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn list_roles(pool: &PgPool, tier_id: i32) -> Result<Vec<i64>> {
    let roles = sqlx::query_scalar!(
        "SELECT role_id FROM tier_roles WHERE tier_id = $1 ORDER BY role_id",
        tier_id
    )
    .fetch_all(pool)
    .await?;
    Ok(roles)
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

pub async fn remove_dungeon(
    pool: &PgPool,
    tier_id: i32,
    dungeon_template_id: i32,
) -> Result<bool> {
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
