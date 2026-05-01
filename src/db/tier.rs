use anyhow::Result;
use sqlx::PgPool;

use crate::db::models::{DungeonTemplate, Tier};

// The `tiers.*` projection used by every read here. Kept as one constant
// so adding a new column doesn't require touching five queries.
const TIER_COLS: &str = "id, guild_id, name, description, runs_channel_id, \
    enable_start_run_ui, start_run_ui_channel_id, \
    start_run_ui_button_message_id, start_run_ui_listing_message_id, \
    hc_idle_minutes, hc_cancel_cooldown_seconds, \
    hc_min_reactors, created_at";

pub async fn create(
    pool: &PgPool,
    guild_id: i64,
    name: &str,
    description: Option<&str>,
) -> Result<Tier> {
    let tier = sqlx::query_as::<_, Tier>(&format!(
        "INSERT INTO tiers (guild_id, name, description)
         VALUES ($1, $2, $3)
         RETURNING {TIER_COLS}"
    ))
    .bind(guild_id)
    .bind(name)
    .bind(description)
    .fetch_one(pool)
    .await?;
    Ok(tier)
}

pub async fn list(pool: &PgPool, guild_id: i64) -> Result<Vec<Tier>> {
    let tiers = sqlx::query_as::<_, Tier>(&format!(
        "SELECT {TIER_COLS} FROM tiers WHERE guild_id = $1 ORDER BY id"
    ))
    .bind(guild_id)
    .fetch_all(pool)
    .await?;
    Ok(tiers)
}

/// Every tier across every guild with `enable_start_run_ui = TRUE`.
/// Used by the boot orphan sweep to repair the sticky button + listing
/// messages.
pub async fn list_start_run_ui_enabled(pool: &PgPool) -> Result<Vec<Tier>> {
    let tiers = sqlx::query_as::<_, Tier>(&format!(
        "SELECT {TIER_COLS} FROM tiers WHERE enable_start_run_ui = TRUE"
    ))
    .fetch_all(pool)
    .await?;
    Ok(tiers)
}

pub async fn get_by_id(pool: &PgPool, id: i32) -> Result<Option<Tier>> {
    let tier = sqlx::query_as::<_, Tier>(&format!("SELECT {TIER_COLS} FROM tiers WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(tier)
}

pub async fn get_by_name(pool: &PgPool, guild_id: i64, name: &str) -> Result<Option<Tier>> {
    let tier = sqlx::query_as::<_, Tier>(&format!(
        "SELECT {TIER_COLS} FROM tiers WHERE guild_id = $1 AND name = $2"
    ))
    .bind(guild_id)
    .bind(name)
    .fetch_optional(pool)
    .await?;
    Ok(tier)
}

pub async fn update(
    pool: &PgPool,
    id: i32,
    name: Option<&str>,
    description: Option<&str>,
    runs_channel_id: Option<i64>,
) -> Result<Option<Tier>> {
    let tier = sqlx::query_as::<_, Tier>(&format!(
        "UPDATE tiers
         SET name            = COALESCE($2, name),
             description     = COALESCE($3, description),
             runs_channel_id = COALESCE($4, runs_channel_id)
         WHERE id = $1
         RETURNING {TIER_COLS}"
    ))
    .bind(id)
    .bind(name)
    .bind(description)
    .bind(runs_channel_id)
    .fetch_optional(pool)
    .await?;
    Ok(tier)
}

/// Update start-run-UI + universal HC-gate knobs on a tier. Each Option
/// is a partial update (NULL = leave alone). Used by the `/setup`
/// start-run-UI sub-step.
#[allow(clippy::too_many_arguments)]
pub async fn update_start_run_ui(
    pool: &PgPool,
    id: i32,
    enabled: Option<bool>,
    channel_id: Option<i64>,
    idle_minutes: Option<i32>,
    cancel_cooldown_seconds: Option<i32>,
    min_reactors: Option<i32>,
) -> Result<Option<Tier>> {
    let tier = sqlx::query_as::<_, Tier>(&format!(
        "UPDATE tiers
         SET enable_start_run_ui        = COALESCE($2, enable_start_run_ui),
             start_run_ui_channel_id    = COALESCE($3, start_run_ui_channel_id),
             hc_idle_minutes            = COALESCE($4, hc_idle_minutes),
             hc_cancel_cooldown_seconds = COALESCE($5, hc_cancel_cooldown_seconds),
             hc_min_reactors            = COALESCE($6, hc_min_reactors)
         WHERE id = $1
         RETURNING {TIER_COLS}"
    ))
    .bind(id)
    .bind(enabled)
    .bind(channel_id)
    .bind(idle_minutes)
    .bind(cancel_cooldown_seconds)
    .bind(min_reactors)
    .fetch_optional(pool)
    .await?;
    Ok(tier)
}

/// Direct setter for the sticky button message ID — bypasses
/// `update_start_run_ui` because sticky-repair runs on every config save
/// and a full COALESCE update is wasteful for a single-column write.
pub async fn set_start_run_ui_button_message(
    pool: &PgPool,
    id: i32,
    message_id: Option<i64>,
) -> Result<()> {
    sqlx::query!(
        "UPDATE tiers SET start_run_ui_button_message_id = $1 WHERE id = $2",
        message_id,
        id
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Direct setter for the sticky listing message ID. See
/// `set_start_run_ui_button_message` for the rationale.
pub async fn set_start_run_ui_listing_message(
    pool: &PgPool,
    id: i32,
    message_id: Option<i64>,
) -> Result<()> {
    sqlx::query!(
        "UPDATE tiers SET start_run_ui_listing_message_id = $1 WHERE id = $2",
        message_id,
        id
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete(pool: &PgPool, id: i32) -> Result<bool> {
    let result = sqlx::query!("DELETE FROM tiers WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

// ---------------------------------------------------------------------------
// Tier ↔ dungeon visibility
//
// Globals are implicitly visible to every tier in every guild and are
// hidden via `tier_dungeon_disables`. Guild-specific templates are
// explicitly attached via `tier_dungeons`. The picker / autocomplete
// query is `(globals NOT IN disables) ∪ (guild-specific IN tier_dungeons)`.
// ---------------------------------------------------------------------------

const TEMPLATE_COLS: &str = "id, guild_id, name, display_name, emoji, color, \
    message_title, message_description, thumbnail_url, image_url, \
    requires_vc, showcase_emoji, created_at";

/// Templates visible in `tier_id` for a guild: implicit globals (minus
/// disables for this tier) plus guild-specific dungeons explicitly
/// attached via `tier_dungeons`. Ordered alphabetically by display_name.
pub async fn list_visible_dungeons(
    pool: &PgPool,
    tier_id: i32,
    guild_id: i64,
) -> Result<Vec<DungeonTemplate>> {
    let rows = sqlx::query_as::<_, DungeonTemplate>(&format!(
        r#"
        SELECT {TEMPLATE_COLS}
        FROM dungeon_templates t
        WHERE
            (t.guild_id IS NULL
             AND NOT EXISTS (
                 SELECT 1 FROM tier_dungeon_disables d
                 WHERE d.tier_id = $1 AND d.dungeon_template_id = t.id
             ))
            OR
            (t.guild_id = $2
             AND EXISTS (
                 SELECT 1 FROM tier_dungeons td
                 WHERE td.tier_id = $1 AND td.dungeon_template_id = t.id
             ))
        ORDER BY t.display_name
        "#
    ))
    .bind(tier_id)
    .bind(guild_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Templates visible in *any* tier of a guild — used by `/headcount`
/// autocomplete, where the chosen tier isn't known until after the user
/// finishes typing the dungeon name. The post-resolution check uses
/// [`is_dungeon_visible`] to reject choices that aren't visible in the
/// resolved tier.
pub async fn list_visible_dungeons_any_tier(
    pool: &PgPool,
    guild_id: i64,
) -> Result<Vec<DungeonTemplate>> {
    let rows = sqlx::query_as::<_, DungeonTemplate>(&format!(
        r#"
        SELECT {TEMPLATE_COLS}
        FROM dungeon_templates t
        WHERE
            (t.guild_id IS NULL
             AND EXISTS (
                 SELECT 1 FROM tiers tr
                 WHERE tr.guild_id = $1
                   AND NOT EXISTS (
                       SELECT 1 FROM tier_dungeon_disables d
                       WHERE d.tier_id = tr.id AND d.dungeon_template_id = t.id
                   )
             ))
            OR
            (t.guild_id = $1
             AND EXISTS (
                 SELECT 1 FROM tier_dungeons td
                     JOIN tiers tr ON tr.id = td.tier_id
                 WHERE tr.guild_id = $1 AND td.dungeon_template_id = t.id
             ))
        ORDER BY t.display_name
        "#
    ))
    .bind(guild_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Whether a single template is visible in `tier_id` for `guild_id`.
/// Used by `/headcount` after tier resolution to reject hidden picks
/// without redoing the full list query.
pub async fn is_dungeon_visible(
    pool: &PgPool,
    tier_id: i32,
    template_id: i32,
    guild_id: i64,
) -> Result<bool> {
    let visible: Option<i32> = sqlx::query_scalar(
        r#"
        SELECT 1
        FROM dungeon_templates t
        WHERE t.id = $2
          AND (
              (t.guild_id IS NULL
               AND NOT EXISTS (
                   SELECT 1 FROM tier_dungeon_disables d
                   WHERE d.tier_id = $1 AND d.dungeon_template_id = t.id
               ))
              OR
              (t.guild_id = $3
               AND EXISTS (
                   SELECT 1 FROM tier_dungeons td
                   WHERE td.tier_id = $1 AND td.dungeon_template_id = t.id
               ))
          )
        "#,
    )
    .bind(tier_id)
    .bind(template_id)
    .bind(guild_id)
    .fetch_optional(pool)
    .await?;
    Ok(visible.is_some())
}

// ---------------------------------------------------------------------------
// Low-level primitives — touch one of the two underlying tables. Higher-
// level callers should prefer [`add_dungeon`] / [`remove_dungeon`] which
// dispatch to the right primitive based on the template's `guild_id`.
// ---------------------------------------------------------------------------

/// Insert into `tier_dungeon_disables` (idempotent). Hides a global from
/// a tier. Returns true iff a row was newly inserted.
pub async fn disable_global_dungeon(pool: &PgPool, tier_id: i32, template_id: i32) -> Result<bool> {
    let result = sqlx::query(
        "INSERT INTO tier_dungeon_disables (tier_id, dungeon_template_id)
         VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(tier_id)
    .bind(template_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Delete from `tier_dungeon_disables`. Re-shows a previously-hidden
/// global. Returns true iff a row was actually deleted.
pub async fn enable_global_dungeon(pool: &PgPool, tier_id: i32, template_id: i32) -> Result<bool> {
    let result = sqlx::query(
        "DELETE FROM tier_dungeon_disables WHERE tier_id = $1 AND dungeon_template_id = $2",
    )
    .bind(tier_id)
    .bind(template_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Insert into `tier_dungeons` (idempotent). Attaches a guild-specific
/// dungeon to a tier. Returns true iff a row was newly inserted.
pub async fn attach_guild_dungeon(pool: &PgPool, tier_id: i32, template_id: i32) -> Result<bool> {
    let result = sqlx::query(
        "INSERT INTO tier_dungeons (tier_id, dungeon_template_id)
         VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(tier_id)
    .bind(template_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Delete from `tier_dungeons`. Hard-detaches a guild-specific dungeon.
/// Returns true iff a row was actually deleted.
pub async fn detach_guild_dungeon(pool: &PgPool, tier_id: i32, template_id: i32) -> Result<bool> {
    let result =
        sqlx::query("DELETE FROM tier_dungeons WHERE tier_id = $1 AND dungeon_template_id = $2")
            .bind(tier_id)
            .bind(template_id)
            .execute(pool)
            .await?;
    Ok(result.rows_affected() > 0)
}

/// Add a dungeon to a tier, dispatching by the template's `guild_id`:
/// globals call [`enable_global_dungeon`], guild-specifics call
/// [`attach_guild_dungeon`]. Returns true iff a state change occurred.
pub async fn add_dungeon(pool: &PgPool, tier_id: i32, template: &DungeonTemplate) -> Result<bool> {
    if template.guild_id.is_none() {
        enable_global_dungeon(pool, tier_id, template.id).await
    } else {
        attach_guild_dungeon(pool, tier_id, template.id).await
    }
}

/// Remove a dungeon from a tier, dispatching by the template's
/// `guild_id`: globals are soft-disabled (insert into
/// `tier_dungeon_disables` so they're hidden but reversible);
/// guild-specifics are hard-detached (delete from `tier_dungeons`).
/// Returns true iff a state change occurred.
pub async fn remove_dungeon(
    pool: &PgPool,
    tier_id: i32,
    template: &DungeonTemplate,
) -> Result<bool> {
    if template.guild_id.is_none() {
        disable_global_dungeon(pool, tier_id, template.id).await
    } else {
        detach_guild_dungeon(pool, tier_id, template.id).await
    }
}
