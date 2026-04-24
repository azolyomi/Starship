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

// ---------------------------------------------------------------------------
// Participants
// ---------------------------------------------------------------------------

/// One entry per (user, declared-item). A user with no declared item has a
/// single row with `dungeon_reaction_id = NULL`; a user declaring multiple
/// items has one row per item.
pub async fn add_participant(
    pool: &PgPool,
    run_id: i32,
    user_id: i64,
    dungeon_reaction_id: Option<i32>,
    confirmed: bool,
) -> Result<()> {
    // COALESCE in the unique index means "no item" collapses to a single row
    // per user; explicit ON CONFLICT on the coalesced expression isn't
    // supported, so we emulate it with NOT EXISTS.
    sqlx::query!(
        r#"
        INSERT INTO run_participants (run_id, user_id, dungeon_reaction_id, confirmed)
        SELECT $1, $2, $3, $4
        WHERE NOT EXISTS (
            SELECT 1 FROM run_participants
            WHERE run_id = $1 AND user_id = $2
              AND COALESCE(dungeon_reaction_id, 0) = COALESCE($3::INT, 0)
        )
        "#,
        run_id,
        user_id,
        dungeon_reaction_id,
        confirmed,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Remove every row for a user (both "no item" and each declared item),
/// used when a user clicks Leave on the run embed.
pub async fn remove_participant_all(pool: &PgPool, run_id: i32, user_id: i64) -> Result<()> {
    sqlx::query!(
        "DELETE FROM run_participants WHERE run_id = $1 AND user_id = $2",
        run_id,
        user_id
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub struct Participant {
    pub user_id: i64,
    pub dungeon_reaction_id: Option<i32>,
    pub confirmed: bool,
}

pub async fn list_participants(pool: &PgPool, run_id: i32) -> Result<Vec<Participant>> {
    let rows = sqlx::query_as!(
        Participant,
        r#"
        SELECT user_id, dungeon_reaction_id, confirmed
        FROM run_participants
        WHERE run_id = $1
        ORDER BY joined_at
        "#,
        run_id
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Distinct users on a run, used by the header count and the transfer-leader
/// select menu (which must exclude the current leader).
pub async fn list_user_ids(pool: &PgPool, run_id: i32) -> Result<Vec<i64>> {
    let ids = sqlx::query_scalar!(
        "SELECT DISTINCT user_id FROM run_participants WHERE run_id = $1 ORDER BY user_id",
        run_id
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}
