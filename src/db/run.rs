use anyhow::Result;
use sqlx::PgPool;

use crate::db::models::Run;

pub async fn create(
    pool: &PgPool,
    guild_id: i64,
    tier_id: i32,
    dungeon_template_id: i32,
    channel_id: i64,
    leader_user_id: i64,
    is_vc_raid: bool,
) -> Result<Run> {
    let row = sqlx::query_as!(
        Run,
        r#"
        INSERT INTO runs
            (guild_id, tier_id, dungeon_template_id,
             channel_id, message_id, leader_user_id, is_vc_raid)
        VALUES ($1, $2, $3, $4, 0, $5, $6)
        RETURNING id, guild_id, tier_id, dungeon_template_id,
                  channel_id, message_id, leader_user_id,
                  location, party, voice_channel_id, is_vc_raid,
                  created_at
        "#,
        guild_id,
        tier_id,
        dungeon_template_id,
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
        SELECT id, guild_id, tier_id, dungeon_template_id,
               channel_id, message_id, leader_user_id,
               location, party, voice_channel_id, is_vc_raid,
               created_at
        FROM runs WHERE id = $1
        "#,
        id
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Delete a run row. Returns true iff this call removed it — used as an
/// atomic "end" claim so two concurrent End clicks only fire the
/// side-effects (VC teardown, message rewrite, audit log) once.
pub async fn delete(pool: &PgPool, id: i32) -> Result<bool> {
    let rows = sqlx::query!("DELETE FROM runs WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(rows.rows_affected() > 0)
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

/// Every live run across every guild. Used by the startup orphan sweep to
/// reconcile DB rows against Discord state.
pub async fn list_all(pool: &PgPool) -> Result<Vec<Run>> {
    let rows = sqlx::query_as!(
        Run,
        r#"
        SELECT id, guild_id, tier_id, dungeon_template_id,
               channel_id, message_id, leader_user_id,
               location, party, voice_channel_id, is_vc_raid,
               created_at
        FROM runs
        "#
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
