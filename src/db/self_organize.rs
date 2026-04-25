//! DB layer for the self-organize raid feature.
//!
//! Two tables are managed here:
//!
//! * `self_organize_slot_claims` — one row per held (tier, dungeon)
//!   slot. A claim is acquired on HC creation, swapped from
//!   `headcount_id` to `run_id` on HC->Run conversion, and released when
//!   the HC is cancelled or the run ends. The PK enforces per-slot
//!   uniqueness; a partial unique index (`idx_so_one_per_user`) enforces
//!   the per-user cap for self-led starts only.
//!
//! * `self_organize_user_cooldowns` — set on self-cancel, checked on the
//!   next "Start a run" click. Lazy-pruned: any expired row observed
//!   during [`cooldown_active`] is deleted before the call returns
//!   `None`, so no background pruner is needed.

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use sqlx::{PgPool, Postgres, Transaction};

use crate::db::models::SlotClaim;

// Single source of truth for the column projection. Mirrors `SlotClaim`
// and is used by every read path so adding a column is one edit.
const CLAIM_COLS: &str = "guild_id, tier_id, dungeon_template_id, leader_user_id, \
    is_self_organized, headcount_id, run_id, acquired_at";

/// Outcome of attempting to acquire a slot claim for a new headcount.
///
/// `Acquired` carries the freshly inserted claim row. `Conflict` carries
/// the existing row (so the caller can render a useful error — "@Alice
/// already has a Lost Halls HC up").
#[derive(Debug)]
pub enum ClaimOutcome {
    /// Slot acquired. The inserted row is returned for symmetry with
    /// `Conflict` (and so future logging/metrics can surface
    /// `acquired_at`); current callers only branch on the variant tag.
    Acquired(#[allow(dead_code)] SlotClaim),
    Conflict(SlotClaim),
}

/// Attempt to acquire the slot for a new headcount.
///
/// Uses `INSERT ... ON CONFLICT DO NOTHING` so two concurrent inserts
/// for the same (guild, tier, dungeon) safely serialize: exactly one
/// returns the inserted row, the other observes the conflict and reads
/// back the holder. The `RETURNING ...` is empty on conflict, which
/// is how we detect the loser.
pub async fn claim_for_headcount(
    tx: &mut Transaction<'_, Postgres>,
    guild_id: i64,
    tier_id: i32,
    dungeon_template_id: i32,
    headcount_id: i32,
    leader_user_id: i64,
    is_self_organized: bool,
) -> Result<ClaimOutcome> {
    // Try to insert. If the PK conflict fires, no row is returned.
    let inserted = sqlx::query_as::<_, SlotClaim>(&format!(
        "INSERT INTO self_organize_slot_claims
            (guild_id, tier_id, dungeon_template_id, leader_user_id,
             is_self_organized, headcount_id, run_id)
         VALUES ($1, $2, $3, $4, $5, $6, NULL)
         ON CONFLICT (guild_id, tier_id, dungeon_template_id) DO NOTHING
         RETURNING {CLAIM_COLS}"
    ))
    .bind(guild_id)
    .bind(tier_id)
    .bind(dungeon_template_id)
    .bind(leader_user_id)
    .bind(is_self_organized)
    .bind(headcount_id)
    .fetch_optional(&mut **tx)
    .await?;

    if let Some(row) = inserted {
        return Ok(ClaimOutcome::Acquired(row));
    }

    // Lost the race. Read back the holder so the caller can name them.
    let holder = sqlx::query_as::<_, SlotClaim>(&format!(
        "SELECT {CLAIM_COLS}
         FROM self_organize_slot_claims
         WHERE guild_id = $1 AND tier_id = $2 AND dungeon_template_id = $3"
    ))
    .bind(guild_id)
    .bind(tier_id)
    .bind(dungeon_template_id)
    .fetch_one(&mut **tx)
    .await?;
    Ok(ClaimOutcome::Conflict(holder))
}

/// Move the claim from headcount-held to run-held without ever releasing
/// the slot lock. Single-row UPDATE that flips which FK is non-null:
/// `(headcount_id=X, run_id=NULL) -> (headcount_id=NULL, run_id=Y)`.
///
/// The CHECK on the table evaluates per-row post-update, so this never
/// transiently violates the "at least one FK non-null" invariant.
///
/// Returns true iff a row was actually swapped (caller can use this as a
/// sanity check; should always be true if the convert path is reached
/// via the proper pre-check).
pub async fn claim_swap_to_run(
    tx: &mut Transaction<'_, Postgres>,
    headcount_id: i32,
    run_id: i32,
) -> Result<bool> {
    let res = sqlx::query!(
        "UPDATE self_organize_slot_claims
            SET headcount_id = NULL, run_id = $2
          WHERE headcount_id = $1",
        headcount_id,
        run_id
    )
    .execute(&mut **tx)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Release a claim that was held by a headcount. Must run *before* the
/// HC row is deleted (the FK is `ON DELETE NO ACTION` precisely so we
/// can't accidentally end up in `(NULL, NULL)` which would violate the
/// CHECK).
pub async fn claim_release_by_headcount(
    tx: &mut Transaction<'_, Postgres>,
    headcount_id: i32,
) -> Result<bool> {
    let res = sqlx::query!(
        "DELETE FROM self_organize_slot_claims WHERE headcount_id = $1",
        headcount_id
    )
    .execute(&mut **tx)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Release a claim that was held by a run. Must run *before* the run
/// row is deleted (same reasoning as `claim_release_by_headcount`).
pub async fn claim_release_by_run(tx: &mut Transaction<'_, Postgres>, run_id: i32) -> Result<bool> {
    let res = sqlx::query!(
        "DELETE FROM self_organize_slot_claims WHERE run_id = $1",
        run_id
    )
    .execute(&mut **tx)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Update the leader on a run-held claim. Called from the transfer-leader
/// flow so the per-user cap follows the new owner: the previous leader
/// becomes free to start another raid, the new leader counts against
/// their own cap.
pub async fn claim_set_leader(
    tx: &mut Transaction<'_, Postgres>,
    run_id: i32,
    new_leader_user_id: i64,
) -> Result<bool> {
    let res = sqlx::query!(
        "UPDATE self_organize_slot_claims
            SET leader_user_id = $2
          WHERE run_id = $1",
        run_id,
        new_leader_user_id
    )
    .execute(&mut **tx)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Read the claim, if any, for a given (guild, tier, dungeon) slot.
/// Used by the anti-troll gate before attempting to acquire.
pub async fn claim_get_by_slot(
    pool: &PgPool,
    guild_id: i64,
    tier_id: i32,
    dungeon_template_id: i32,
) -> Result<Option<SlotClaim>> {
    let row = sqlx::query_as::<_, SlotClaim>(&format!(
        "SELECT {CLAIM_COLS}
         FROM self_organize_slot_claims
         WHERE guild_id = $1 AND tier_id = $2 AND dungeon_template_id = $3"
    ))
    .bind(guild_id)
    .bind(tier_id)
    .bind(dungeon_template_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Count how many self-organized claims (HC or Run) a user holds in a
/// guild. Backs the "one active raid per user" cap. Filters to
/// `is_self_organized = TRUE` so staff-led raids in self-organize tiers
/// don't count against the leader's quota.
pub async fn claim_count_for_user(pool: &PgPool, guild_id: i64, user_id: i64) -> Result<i64> {
    let count: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM self_organize_slot_claims
          WHERE guild_id = $1 AND leader_user_id = $2 AND is_self_organized = TRUE",
        guild_id,
        user_id
    )
    .fetch_one(pool)
    .await?
    .unwrap_or(0);
    Ok(count)
}

/// All claims in a guild. Used by the listing renderer (per-tier filter
/// applied in service code) and by the boot orphan-sweep reconcile pass.
pub async fn claim_list_for_guild(pool: &PgPool, guild_id: i64) -> Result<Vec<SlotClaim>> {
    let rows = sqlx::query_as::<_, SlotClaim>(&format!(
        "SELECT {CLAIM_COLS}
         FROM self_organize_slot_claims
         WHERE guild_id = $1
         ORDER BY acquired_at ASC"
    ))
    .bind(guild_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// All claims, across every guild. Used by the boot orphan-sweep
/// reconcile pass to find dangling claim rows whose HC/Run was deleted
/// while the bot was offline.
pub async fn claim_list_all(pool: &PgPool) -> Result<Vec<SlotClaim>> {
    let rows = sqlx::query_as::<_, SlotClaim>(&format!(
        "SELECT {CLAIM_COLS} FROM self_organize_slot_claims"
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Force-delete a claim by primary key. Used only by the orphan sweep
/// when both linked HC and Run rows are gone but the claim somehow
/// survived (shouldn't happen with `ON DELETE NO ACTION` + transactional
/// release, but defence in depth covers manual DB surgery).
pub async fn claim_force_delete(
    pool: &PgPool,
    guild_id: i64,
    tier_id: i32,
    dungeon_template_id: i32,
) -> Result<bool> {
    let res = sqlx::query!(
        "DELETE FROM self_organize_slot_claims
          WHERE guild_id = $1 AND tier_id = $2 AND dungeon_template_id = $3",
        guild_id,
        tier_id,
        dungeon_template_id
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

// ---- cooldowns -------------------------------------------------------------

/// Record (or extend) a per-(guild, tier, user) cooldown. Called when a
/// leader cancels their *own* self-organized HC.
pub async fn cooldown_set(
    pool: &PgPool,
    guild_id: i64,
    tier_id: i32,
    user_id: i64,
    duration: Duration,
) -> Result<()> {
    let until = Utc::now() + duration;
    sqlx::query!(
        "INSERT INTO self_organize_user_cooldowns
            (guild_id, tier_id, user_id, expires_at)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (guild_id, tier_id, user_id) DO UPDATE
            SET expires_at = EXCLUDED.expires_at",
        guild_id,
        tier_id,
        user_id,
        until,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Return the cooldown expiry for a user-tier, or `None` if none is
/// active. Lazy-prunes: an expired row observed by this query is
/// deleted before returning, so the table self-cleans on read.
pub async fn cooldown_active(
    pool: &PgPool,
    guild_id: i64,
    tier_id: i32,
    user_id: i64,
) -> Result<Option<DateTime<Utc>>> {
    let row = sqlx::query!(
        "SELECT expires_at FROM self_organize_user_cooldowns
          WHERE guild_id = $1 AND tier_id = $2 AND user_id = $3",
        guild_id,
        tier_id,
        user_id,
    )
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else { return Ok(None) };

    if row.expires_at > Utc::now() {
        return Ok(Some(row.expires_at));
    }

    // Expired: prune lazily so the table stays small without a
    // background sweeper.
    sqlx::query!(
        "DELETE FROM self_organize_user_cooldowns
          WHERE guild_id = $1 AND tier_id = $2 AND user_id = $3",
        guild_id,
        tier_id,
        user_id,
    )
    .execute(pool)
    .await?;
    Ok(None)
}
