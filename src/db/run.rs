use anyhow::Result;
use sqlx::{PgPool, Postgres, Transaction};

use crate::db::models::Run;

// The `runs.*` projection used by every read here. Single source of
// truth so adding a column doesn't require touching multiple queries.
const RUN_COLS: &str = "id, guild_id, tier_id, dungeon_template_id, \
    channel_id, message_id, leader_user_id, location, party, \
    voice_channel_id, is_vc_raid, is_self_organized, created_at";

/// Insert a run row inside an existing transaction. The HC->Run convert
/// path swaps the slot-claim FK from `headcount_id` to `run_id` in the
/// same tx that creates the run, so the slot lock never gets released.
#[allow(clippy::too_many_arguments)]
pub async fn create_tx(
    tx: &mut Transaction<'_, Postgres>,
    guild_id: i64,
    tier_id: i32,
    dungeon_template_id: i32,
    channel_id: i64,
    leader_user_id: i64,
    is_vc_raid: bool,
    is_self_organized: bool,
) -> Result<Run> {
    let row = sqlx::query_as::<_, Run>(&format!(
        "INSERT INTO runs
            (guild_id, tier_id, dungeon_template_id,
             channel_id, message_id, leader_user_id, is_vc_raid, is_self_organized)
         VALUES ($1, $2, $3, $4, 0, $5, $6, $7)
         RETURNING {RUN_COLS}"
    ))
    .bind(guild_id)
    .bind(tier_id)
    .bind(dungeon_template_id)
    .bind(channel_id)
    .bind(leader_user_id)
    .bind(is_vc_raid)
    .bind(is_self_organized)
    .fetch_one(&mut **tx)
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
    let row = sqlx::query_as::<_, Run>(&format!("SELECT {RUN_COLS} FROM runs WHERE id = $1"))
        .bind(id)
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

/// Transactional variant of [`delete`]. Pairs with the slot-claim release
/// in the self-organize flow.
pub async fn delete_tx(tx: &mut Transaction<'_, Postgres>, id: i32) -> Result<bool> {
    let rows = sqlx::query!("DELETE FROM runs WHERE id = $1", id)
        .execute(&mut **tx)
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

/// Transactional setter so the leader change can ride in the same tx as
/// the self-organize claim's `claim_set_leader` — keeps the per-user cap
/// in sync without exposing a window where the run and the claim disagree
/// on who the leader is.
pub async fn set_leader_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: i32,
    leader_user_id: i64,
) -> Result<()> {
    sqlx::query!(
        "UPDATE runs SET leader_user_id = $1 WHERE id = $2",
        leader_user_id,
        id
    )
    .execute(&mut **tx)
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
    let rows = sqlx::query_as::<_, Run>(&format!("SELECT {RUN_COLS} FROM runs"))
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Runs whose `created_at` is older than `cutoff` — used by the periodic
/// idle-timeout sweep to find raids that the leader forgot to end.
pub async fn list_created_before(
    pool: &PgPool,
    cutoff: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<Run>> {
    let rows = sqlx::query_as::<_, Run>(&format!(
        "SELECT {RUN_COLS} FROM runs WHERE created_at < $1"
    ))
    .bind(cutoff)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
