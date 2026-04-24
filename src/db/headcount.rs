use anyhow::Result;
use sqlx::PgPool;

use crate::db::models::Headcount;

pub async fn create(
    pool: &PgPool,
    guild_id: i64,
    tier_id: i32,
    dungeon_template_id: i32,
    channel_id: i64,
    leader_user_id: i64,
) -> Result<Headcount> {
    let row = sqlx::query_as::<_, Headcount>(
        r#"
        INSERT INTO headcounts
            (guild_id, tier_id, dungeon_template_id, channel_id, message_id, leader_user_id)
        VALUES ($1, $2, $3, $4, 0, $5)
        RETURNING *
        "#,
    )
    .bind(guild_id)
    .bind(tier_id)
    .bind(dungeon_template_id)
    .bind(channel_id)
    .bind(leader_user_id)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn set_message_id(pool: &PgPool, id: i32, message_id: i64) -> Result<()> {
    sqlx::query("UPDATE headcounts SET message_id = $1 WHERE id = $2")
        .bind(message_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get(pool: &PgPool, id: i32) -> Result<Option<Headcount>> {
    let row = sqlx::query_as::<_, Headcount>("SELECT * FROM headcounts WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

pub async fn set_status(pool: &PgPool, id: i32, status: &str) -> Result<()> {
    sqlx::query("UPDATE headcounts SET status = $1 WHERE id = $2")
        .bind(status)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn list_active(pool: &PgPool, guild_id: i64) -> Result<Vec<Headcount>> {
    let rows = sqlx::query_as::<_, Headcount>(
        "SELECT * FROM headcounts WHERE guild_id = $1 AND status = 'active'",
    )
    .bind(guild_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

