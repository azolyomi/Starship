//! Row-level queries for the verification flow.
//!
//! Two tables:
//!   * `verifications`     — pending attempts (code issued, awaiting check)
//!   * `verified_users`    — completed bindings (Discord user ↔ IGN)
//!
//! Higher-level orchestration (issue → wait → check → assign role +
//! nickname) lives in `services::verification`. This module only owns the
//! SQL.

use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::db::models::{PendingVerification, VerifiedUser};

// ---------------------------------------------------------------------------
// Pending verifications
// ---------------------------------------------------------------------------

/// Insert (or replace) the pending-verification row for one user. The PK is
/// (guild_id, discord_user_id), so a second /verify by the same user
/// silently overwrites the prior code — no per-user code accumulation.
pub async fn upsert_pending(
    pool: &PgPool,
    guild_id: i64,
    discord_user_id: i64,
    claimed_ign: &str,
    code: &str,
    expires_at: DateTime<Utc>,
) -> Result<()> {
    sqlx::query!(
        "INSERT INTO verifications
            (guild_id, discord_user_id, claimed_ign, code, expires_at)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (guild_id, discord_user_id) DO UPDATE
            SET claimed_ign = EXCLUDED.claimed_ign,
                code        = EXCLUDED.code,
                expires_at  = EXCLUDED.expires_at,
                created_at  = NOW()",
        guild_id,
        discord_user_id,
        claimed_ign,
        code,
        expires_at,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Read the pending row for one user, if any.
pub async fn get_pending(
    pool: &PgPool,
    guild_id: i64,
    discord_user_id: i64,
) -> Result<Option<PendingVerification>> {
    let row = sqlx::query_as!(
        PendingVerification,
        "SELECT guild_id, discord_user_id, claimed_ign, code, expires_at, created_at
         FROM verifications
         WHERE guild_id = $1 AND discord_user_id = $2",
        guild_id,
        discord_user_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Drop the pending row for one user. Idempotent — no-op if absent. Called
/// after a successful check (the user is now in `verified_users`) and as
/// part of admin recovery flows.
pub async fn delete_pending(pool: &PgPool, guild_id: i64, discord_user_id: i64) -> Result<()> {
    sqlx::query!(
        "DELETE FROM verifications
         WHERE guild_id = $1 AND discord_user_id = $2",
        guild_id,
        discord_user_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Bulk-delete every pending row whose `expires_at` is in the past. Called
/// once at startup from `services::orphan_sweep`. Returns the row count
/// for logging.
pub async fn delete_expired(pool: &PgPool) -> Result<u64> {
    let res = sqlx::query!("DELETE FROM verifications WHERE expires_at < NOW()")
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

// ---------------------------------------------------------------------------
// Completed verifications
// ---------------------------------------------------------------------------

/// Atomic completion: delete the pending row + UPSERT the verified-users
/// row in a single transaction. Returns the prior IGN (if any) so the
/// caller can render a "rebind" message.
///
/// `verified_by` is `None` for self-verifies and `Some(admin_user_id)` for
/// `/mv`. The caller is responsible for the realmeye-side check; this
/// function does not touch `verifications.code`.
///
/// On UNIQUE conflict against `(guild_id, realmeye_ign)`, returns
/// [`UpsertResult::IgnTaken { holder }`] with the existing holder's
/// Discord user ID — the caller surfaces a friendly error and does not
/// retry. Anything else propagates.
pub async fn complete(
    pool: &PgPool,
    guild_id: i64,
    discord_user_id: i64,
    realmeye_ign: &str,
    verified_by: Option<i64>,
) -> Result<UpsertResult> {
    let mut tx = pool.begin().await?;

    // If another Discord user in this guild already holds this IGN,
    // reject before we touch anything. Catching the unique violation
    // after the UPSERT would also work, but a pre-check lets us return
    // the conflicting holder's ID for a more helpful message.
    let existing_holder: Option<i64> = sqlx::query_scalar!(
        "SELECT discord_user_id FROM verified_users
         WHERE guild_id = $1 AND realmeye_ign = $2 AND discord_user_id <> $3",
        guild_id,
        realmeye_ign,
        discord_user_id,
    )
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(holder) = existing_holder {
        return Ok(UpsertResult::IgnTaken { holder });
    }

    // Capture the prior IGN (if any) so the caller can render rebind UX.
    let prior_ign: Option<String> = sqlx::query_scalar!(
        "SELECT realmeye_ign FROM verified_users
         WHERE guild_id = $1 AND discord_user_id = $2",
        guild_id,
        discord_user_id,
    )
    .fetch_optional(&mut *tx)
    .await?;

    sqlx::query!(
        "INSERT INTO verified_users
            (guild_id, discord_user_id, realmeye_ign, verified_at, verified_by)
         VALUES ($1, $2, $3, NOW(), $4)
         ON CONFLICT (guild_id, discord_user_id) DO UPDATE
            SET realmeye_ign = EXCLUDED.realmeye_ign,
                verified_at  = NOW(),
                verified_by  = EXCLUDED.verified_by",
        guild_id,
        discord_user_id,
        realmeye_ign,
        verified_by,
    )
    .execute(&mut *tx)
    .await?;

    sqlx::query!(
        "DELETE FROM verifications
         WHERE guild_id = $1 AND discord_user_id = $2",
        guild_id,
        discord_user_id,
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(match prior_ign {
        Some(prior) if prior != realmeye_ign => UpsertResult::Rebound {
            from: prior,
            to: realmeye_ign.to_string(),
        },
        Some(_) => UpsertResult::Refreshed,
        None => UpsertResult::Created,
    })
}

/// Outcome of [`complete`]. Distinguishes the three semantically distinct
/// success cases (so the caller can render an honest message) from the
/// one expected failure (IGN already held by someone else).
#[derive(Debug)]
pub enum UpsertResult {
    /// Fresh verification — no prior row for this user.
    Created,
    /// User re-verified to the same IGN (e.g. /mv overrode then user
    /// re-ran /verify with the same name). `verified_at` was bumped.
    Refreshed,
    /// User was already verified to a different IGN, now rebound.
    Rebound { from: String, to: String },
    /// Another Discord user in this guild is already verified as this
    /// IGN. The caller refuses and reports `holder`.
    IgnTaken { holder: i64 },
}

/// Read the verified-users row for one user, if any.
#[allow(dead_code)] // exposed for future /unverify, /whois, log embeds
pub async fn get_verified(
    pool: &PgPool,
    guild_id: i64,
    discord_user_id: i64,
) -> Result<Option<VerifiedUser>> {
    let row = sqlx::query_as!(
        VerifiedUser,
        "SELECT guild_id, discord_user_id, realmeye_ign,
                verified_at, verified_by
         FROM verified_users
         WHERE guild_id = $1 AND discord_user_id = $2",
        guild_id,
        discord_user_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}
