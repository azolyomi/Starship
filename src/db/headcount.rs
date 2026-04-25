use anyhow::Result;
use sqlx::{PgPool, Postgres, Transaction};

use crate::db::models::Headcount;

/// Insert a fresh headcount inside a caller-provided transaction so the
/// row can be paired with a self-organize slot-claim insert without
/// exposing a half-written state to other connections.
// Phase D will introduce a `NewHeadcount` parameter struct alongside the
// snowflake-newtype migration; collapsing now would churn every caller for
// purely cosmetic reasons.
#[allow(clippy::too_many_arguments)]
pub async fn create_tx(
    tx: &mut Transaction<'_, Postgres>,
    guild_id: i64,
    tier_id: i32,
    dungeon_template_id: i32,
    channel_id: i64,
    leader_user_id: i64,
    location: Option<&str>,
    party: Option<&str>,
    is_self_organized: bool,
) -> Result<Headcount> {
    let row = sqlx::query_as::<_, Headcount>(
        r#"
        INSERT INTO headcounts
            (guild_id, tier_id, dungeon_template_id, channel_id, message_id,
             leader_user_id, location, party, is_self_organized)
        VALUES ($1, $2, $3, $4, 0, $5, $6, $7, $8)
        RETURNING *
        "#,
    )
    .bind(guild_id)
    .bind(tier_id)
    .bind(dungeon_template_id)
    .bind(channel_id)
    .bind(leader_user_id)
    .bind(location)
    .bind(party)
    .bind(is_self_organized)
    .fetch_one(&mut **tx)
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

/// Stash the leader's intended location + party on the row at HC create
/// time so the HC->Run convert modal can pre-fill them. Used by the
/// self-organize flow, which collects these inputs in its own modal
/// before the HC posts. A single `UPDATE` saves a round-trip vs. two
/// setters and avoids partially-written rows on connection failure.
pub async fn set_location_and_party(
    pool: &PgPool,
    id: i32,
    location: Option<&str>,
    party: Option<&str>,
) -> Result<()> {
    sqlx::query("UPDATE headcounts SET location = $1, party = $2 WHERE id = $3")
        .bind(location)
        .bind(party)
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

/// Delete a headcount row. Returns true iff this call removed it — callers
/// use the return value as an atomic claim: two concurrent Start/Cancel
/// clicks both run, but only one gets `true` and proceeds with the
/// side-effectful work (posting a run, editing the message).
pub async fn delete(pool: &PgPool, id: i32) -> Result<bool> {
    let rows = sqlx::query("DELETE FROM headcounts WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(rows.rows_affected() > 0)
}

/// Transactional variant of [`delete`]. Same atomic-claim semantic but
/// bound to a tx so the caller can pair it with a slot-claim release
/// (which must happen in the same tx because of the `ON DELETE NO ACTION`
/// FK from the claim row).
pub async fn delete_tx(tx: &mut Transaction<'_, Postgres>, id: i32) -> Result<bool> {
    let rows = sqlx::query("DELETE FROM headcounts WHERE id = $1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(rows.rows_affected() > 0)
}

/// Every live headcount across every guild. Used by the startup orphan
/// sweep to reconcile DB rows against Discord state.
pub async fn list_all(pool: &PgPool) -> Result<Vec<Headcount>> {
    let rows = sqlx::query_as::<_, Headcount>("SELECT * FROM headcounts")
        .fetch_all(pool)
        .await?;
    Ok(rows)
}
