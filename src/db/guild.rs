use anyhow::Result;
use sqlx::PgPool;

use crate::db::models::Guild;

pub async fn get(pool: &PgPool, guild_id: i64) -> Result<Option<Guild>> {
    let guild = sqlx::query_as!(
        Guild,
        "SELECT guild_id, log_channel_id, superadmin_user_id,
                setup_complete, loot_tier_threshold,
                verified_role_id, verify_channel_id, verify_message_id,
                created_at, updated_at
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
         RETURNING guild_id, log_channel_id, superadmin_user_id,
                   setup_complete, loot_tier_threshold,
                   verified_role_id, verify_channel_id, verify_message_id,
                   created_at, updated_at",
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

pub async fn set_verified_role(pool: &PgPool, guild_id: i64, role_id: Option<i64>) -> Result<()> {
    sqlx::query!(
        "UPDATE guilds SET verified_role_id = $2 WHERE guild_id = $1",
        guild_id,
        role_id
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_verify_channel(
    pool: &PgPool,
    guild_id: i64,
    channel_id: Option<i64>,
) -> Result<()> {
    sqlx::query!(
        "UPDATE guilds SET verify_channel_id = $2 WHERE guild_id = $1",
        guild_id,
        channel_id
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Every guild with a posted Verify message, returned as
/// `(guild_id, channel_id, message_id)`. Used by the startup sweep to
/// detect deleted persistent messages and null out the message ID. Both
/// channel + message are NOT NULL when message is set.
pub async fn list_verify_messages(pool: &PgPool) -> Result<Vec<(i64, i64, i64)>> {
    let rows = sqlx::query!(
        "SELECT guild_id, verify_channel_id, verify_message_id
         FROM guilds
         WHERE verify_message_id IS NOT NULL
           AND verify_channel_id IS NOT NULL"
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            // Both verified non-null by the WHERE clause.
            (
                r.guild_id,
                r.verify_channel_id.expect("verify_channel_id non-null"),
                r.verify_message_id.expect("verify_message_id non-null"),
            )
        })
        .collect())
}

pub async fn set_verify_message(
    pool: &PgPool,
    guild_id: i64,
    message_id: Option<i64>,
) -> Result<()> {
    sqlx::query!(
        "UPDATE guilds SET verify_message_id = $2 WHERE guild_id = $1",
        guild_id,
        message_id
    )
    .execute(pool)
    .await?;
    Ok(())
}
