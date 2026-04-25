use anyhow::Result;
use sqlx::PgPool;

use crate::db::models::Permission;

pub async fn grant(
    pool: &PgPool,
    guild_id: i64,
    role_id: i64,
    action: &str,
    tier_id: Option<i32>,
    dungeon_template_id: Option<i32>,
) -> Result<bool> {
    let result = sqlx::query!(
        "INSERT INTO permissions (guild_id, role_id, action, tier_id, dungeon_template_id)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT DO NOTHING",
        guild_id,
        role_id,
        action,
        tier_id,
        dungeon_template_id
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn revoke(
    pool: &PgPool,
    guild_id: i64,
    role_id: i64,
    action: &str,
    tier_id: Option<i32>,
    dungeon_template_id: Option<i32>,
) -> Result<bool> {
    let result = sqlx::query!(
        "DELETE FROM permissions
         WHERE guild_id = $1
           AND role_id = $2
           AND action = $3
           AND COALESCE(tier_id, 0) = COALESCE($4, 0)
           AND COALESCE(dungeon_template_id, 0) = COALESCE($5, 0)",
        guild_id,
        role_id,
        action,
        tier_id,
        dungeon_template_id
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn list_for_guild(pool: &PgPool, guild_id: i64) -> Result<Vec<Permission>> {
    let rows = sqlx::query_as!(
        Permission,
        "SELECT id, guild_id, role_id, action, tier_id, dungeon_template_id
         FROM permissions
         WHERE guild_id = $1
         ORDER BY action, role_id",
        guild_id
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Roles that hold the `StartHeadcount` grant scoped exactly to this tier.
/// Used by the setup wizard to render "current leader roles" — the wizard
/// keeps the seven `LEADER_ACTIONS` in lockstep, so any one of them is a
/// reliable proxy for "this role is a leader of this tier".
pub async fn list_leader_roles_for_tier(pool: &PgPool, tier_id: i32) -> Result<Vec<i64>> {
    let roles = sqlx::query_scalar!(
        "SELECT DISTINCT role_id FROM permissions
         WHERE tier_id = $1 AND action = 'StartHeadcount'
         ORDER BY role_id",
        tier_id
    )
    .fetch_all(pool)
    .await?;
    Ok(roles)
}

/// Returns true if any role in `role_ids` has `action` in this guild.
/// A grant with NULL tier_id matches any tier; NULL dungeon_template_id matches any dungeon.
pub async fn check(
    pool: &PgPool,
    guild_id: i64,
    role_ids: &[i64],
    action: &str,
    tier_id: Option<i32>,
    dungeon_template_id: Option<i32>,
) -> Result<bool> {
    if role_ids.is_empty() {
        return Ok(false);
    }
    let found: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM permissions
            WHERE guild_id = $1
              AND role_id = ANY($2)
              AND action = $3
              AND (tier_id IS NULL OR tier_id = $4)
              AND (dungeon_template_id IS NULL OR dungeon_template_id = $5)
        )",
    )
    .bind(guild_id)
    .bind(role_ids)
    .bind(action)
    .bind(tier_id)
    .bind(dungeon_template_id)
    .fetch_one(pool)
    .await?;
    Ok(found)
}
