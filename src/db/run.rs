use anyhow::Result;
use sqlx::PgPool;

use crate::db::models::Run;

pub async fn create(
    pool: &PgPool,
    guild_id: i64,
    tier_id: i32,
    dungeon_template_id: i32,
    headcount_id: Option<i32>,
    channel_id: i64,
    leader_user_id: i64,
    is_vc_raid: bool,
) -> Result<Run> {
    let row = sqlx::query_as!(
        Run,
        r#"
        INSERT INTO runs
            (guild_id, tier_id, dungeon_template_id, headcount_id,
             channel_id, message_id, leader_user_id, is_vc_raid)
        VALUES ($1, $2, $3, $4, $5, 0, $6, $7)
        RETURNING id, guild_id, tier_id, dungeon_template_id, headcount_id,
                  channel_id, message_id, leader_user_id,
                  location, party, voice_channel_id, is_vc_raid,
                  status, created_at, ended_at
        "#,
        guild_id,
        tier_id,
        dungeon_template_id,
        headcount_id,
        channel_id,
        leader_user_id,
        is_vc_raid,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn set_message_id(pool: &PgPool, id: i32, message_id: i64) -> Result<()> {
    sqlx::query!(
        "UPDATE runs SET message_id = $1 WHERE id = $2",
        message_id,
        id
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get(pool: &PgPool, id: i32) -> Result<Option<Run>> {
    let row = sqlx::query_as!(
        Run,
        r#"
        SELECT id, guild_id, tier_id, dungeon_template_id, headcount_id,
               channel_id, message_id, leader_user_id,
               location, party, voice_channel_id, is_vc_raid,
               status, created_at, ended_at
        FROM runs WHERE id = $1
        "#,
        id
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn list_active(pool: &PgPool, guild_id: i64) -> Result<Vec<Run>> {
    let rows = sqlx::query_as!(
        Run,
        r#"
        SELECT id, guild_id, tier_id, dungeon_template_id, headcount_id,
               channel_id, message_id, leader_user_id,
               location, party, voice_channel_id, is_vc_raid,
               status, created_at, ended_at
        FROM runs WHERE guild_id = $1 AND status = 'active'
        ORDER BY created_at
        "#,
        guild_id
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn set_status(pool: &PgPool, id: i32, status: &str) -> Result<()> {
    // When transitioning to 'ended', stamp ended_at at the same time so the
    // two fields can't drift.
    sqlx::query!(
        r#"
        UPDATE runs
        SET status = $1,
            ended_at = CASE WHEN $1 = 'ended' THEN NOW() ELSE ended_at END
        WHERE id = $2
        "#,
        status,
        id
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_location(pool: &PgPool, id: i32, location: Option<&str>) -> Result<()> {
    sqlx::query!("UPDATE runs SET location = $1 WHERE id = $2", location, id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_party(pool: &PgPool, id: i32, party: Option<&str>) -> Result<()> {
    sqlx::query!("UPDATE runs SET party = $1 WHERE id = $2", party, id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_leader(pool: &PgPool, id: i32, leader_user_id: i64) -> Result<()> {
    sqlx::query!(
        "UPDATE runs SET leader_user_id = $1 WHERE id = $2",
        leader_user_id,
        id
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_voice_channel(
    pool: &PgPool,
    id: i32,
    voice_channel_id: Option<i64>,
) -> Result<()> {
    sqlx::query!(
        "UPDATE runs SET voice_channel_id = $1 WHERE id = $2",
        voice_channel_id,
        id
    )
    .execute(pool)
    .await?;
    Ok(())
}
