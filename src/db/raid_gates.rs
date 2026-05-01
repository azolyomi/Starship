//! DB layer for the universal headcount-protection gates.
//!
//! Slot lock is enforced directly by a UNIQUE index on
//! `headcounts(guild_id, tier_id, dungeon_template_id)` — no auxiliary
//! claim table is needed. Per-user cap lives in
//! [`crate::db::headcount::count_active_raids_for_user`]. The remaining
//! gate-state — the post-cancel cooldown — lives here.
//!
//! Cooldowns are written when a leader cancels their *own* HC and read on
//! the next `/hc` (or sticky-button) start. Lazy-pruned: any expired row
//! observed during [`cooldown_active`] is deleted before the call returns
//! `None`, so no background pruner is needed.

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;

/// Record (or extend) a per-(guild, tier, user) cooldown. Called when a
/// leader cancels their own headcount.
pub async fn cooldown_set(
    pool: &PgPool,
    guild_id: i64,
    tier_id: i32,
    user_id: i64,
    duration: Duration,
) -> Result<()> {
    let until = Utc::now() + duration;
    sqlx::query!(
        "INSERT INTO hc_user_cooldowns
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
        "SELECT expires_at FROM hc_user_cooldowns
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
        "DELETE FROM hc_user_cooldowns
          WHERE guild_id = $1 AND tier_id = $2 AND user_id = $3",
        guild_id,
        tier_id,
        user_id,
    )
    .execute(pool)
    .await?;
    Ok(None)
}
