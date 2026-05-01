use anyhow::Result;
use sqlx::{PgPool, Postgres, Transaction};

use crate::db::models::Headcount;

/// Outcome of [`create_tx`]. The slot lock — a UNIQUE index on
/// `(guild_id, tier_id, dungeon_template_id)` — surfaces concurrent
/// duplicate starts as `SlotInUse` rather than a SQL error bubbling all
/// the way up. The caller renders a friendly "another raid is up"
/// message and never partially commits.
pub enum CreateOutcome {
    Created(Headcount),
    SlotInUse,
}

/// Insert a fresh headcount inside a caller-provided transaction.
///
/// Returns [`CreateOutcome::SlotInUse`] when the per-(guild, tier,
/// dungeon) UNIQUE index rejects the insert because a live HC already
/// exists for that slot. All other DB errors bubble normally.
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
) -> Result<CreateOutcome> {
    let result = sqlx::query_as::<_, Headcount>(
        r#"
        INSERT INTO headcounts
            (guild_id, tier_id, dungeon_template_id, channel_id, message_id,
             leader_user_id, location, party)
        VALUES ($1, $2, $3, $4, 0, $5, $6, $7)
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
    .fetch_one(&mut **tx)
    .await;

    match result {
        Ok(row) => Ok(CreateOutcome::Created(row)),
        Err(sqlx::Error::Database(db_err)) if is_unique_violation(db_err.as_ref()) => {
            Ok(CreateOutcome::SlotInUse)
        }
        Err(e) => Err(e.into()),
    }
}

/// Postgres SQLSTATE for unique_violation. We translate this single class
/// of failure into a domain-level outcome; all other DB errors propagate.
fn is_unique_violation(err: &(dyn sqlx::error::DatabaseError + 'static)) -> bool {
    err.code().as_deref() == Some("23505")
}

/// Look up the leader of an active HC for a slot, if any. Used to render
/// "another raid is up (led by @user)" when a /hc start lost the race.
pub async fn slot_holder(
    pool: &PgPool,
    guild_id: i64,
    tier_id: i32,
    dungeon_template_id: i32,
) -> Result<Option<Headcount>> {
    let row = sqlx::query_as::<_, Headcount>(
        "SELECT * FROM headcounts
         WHERE guild_id = $1 AND tier_id = $2 AND dungeon_template_id = $3",
    )
    .bind(guild_id)
    .bind(tier_id)
    .bind(dungeon_template_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Count the active raids (headcounts + runs) led by `user_id` in this
/// guild. Drives the per-user cap enforced before every `/hc` start;
/// admins/`ManageRuns` bypass the cap at the service layer.
pub async fn count_active_raids_for_user(
    pool: &PgPool,
    guild_id: i64,
    user_id: i64,
) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "SELECT
            (SELECT COUNT(*) FROM headcounts WHERE guild_id = $1 AND leader_user_id = $2)
          + (SELECT COUNT(*) FROM runs       WHERE guild_id = $1 AND leader_user_id = $2)",
    )
    .bind(guild_id)
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    Ok(count)
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
/// start-run UI flow, which collects these inputs in its own modal
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
/// bound to a tx so the caller can pair it with the run insert on the
/// HC->Run convert path: the HC delete frees the slot lock and the run
/// insert commits in the same tx, so a concurrent /hc for the same
/// dungeon either sees the old HC (and gets SlotInUse) or sees the new
/// run (and is allowed, since runs don't hold the slot).
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

/// Live headcounts for a single tier, newest first. Used by the
/// start-run-UI listing renderer.
pub async fn list_for_tier(pool: &PgPool, tier_id: i32) -> Result<Vec<Headcount>> {
    let rows = sqlx::query_as::<_, Headcount>(
        "SELECT * FROM headcounts WHERE tier_id = $1 ORDER BY created_at DESC",
    )
    .bind(tier_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Headcounts whose `created_at` is older than `cutoff` — used by the
/// periodic idle-timeout sweep to find HCs that the leader forgot to
/// convert or cancel. Mirrors `db::run::list_created_before`.
pub async fn list_created_before(
    pool: &PgPool,
    cutoff: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<Headcount>> {
    let rows = sqlx::query_as::<_, Headcount>("SELECT * FROM headcounts WHERE created_at < $1")
        .bind(cutoff)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}
