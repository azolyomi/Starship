use anyhow::Result;
use sqlx::PgPool;

use crate::db::models::Guild;

pub async fn get(pool: &PgPool, guild_id: i64) -> Result<Option<Guild>> {
    let guild = sqlx::query_as!(
        Guild,
        "SELECT guild_id, log_channel_id, notification_channel_id, superadmin_user_id,
                setup_complete, created_at, updated_at
         FROM guilds WHERE guild_id = $1",
        guild_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(guild)
}

pub async fn upsert(pool: &PgPool, guild_id: i64) -> Result<Guild> {
    let guild = sqlx::query_as!(
        Guild,
        "INSERT INTO guilds (guild_id)
         VALUES ($1)
         ON CONFLICT (guild_id) DO UPDATE SET updated_at = NOW()
         RETURNING guild_id, log_channel_id, notification_channel_id, superadmin_user_id,
                   setup_complete, created_at, updated_at",
        guild_id
    )
    .fetch_one(pool)
    .await?;
    Ok(guild)
}

pub async fn set_superadmin(pool: &PgPool, guild_id: i64, user_id: Option<i64>) -> Result<()> {
    sqlx::query!(
        "UPDATE guilds SET superadmin_user_id = $2 WHERE guild_id = $1",
        guild_id,
        user_id
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_setup_complete(pool: &PgPool, guild_id: i64, complete: bool) -> Result<()> {
    sqlx::query!(
        "UPDATE guilds SET setup_complete = $2 WHERE guild_id = $1",
        guild_id,
        complete
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_log_channel(pool: &PgPool, guild_id: i64, channel_id: Option<i64>) -> Result<()> {
    sqlx::query!(
        "UPDATE guilds SET log_channel_id = $2 WHERE guild_id = $1",
        guild_id,
        channel_id
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_notification_channel(
    pool: &PgPool,
    guild_id: i64,
    channel_id: Option<i64>,
) -> Result<()> {
    sqlx::query!(
        "UPDATE guilds SET notification_channel_id = $2 WHERE guild_id = $1",
        guild_id,
        channel_id
    )
    .execute(pool)
    .await?;
    Ok(())
}
