use anyhow::Result;
use sqlx::PgPool;

use crate::db::models::{DungeonReaction, DungeonTemplate};

/// All templates visible to a guild: guild-specific rows override global ones by name.
pub async fn list_for_guild(pool: &PgPool, guild_id: i64) -> Result<Vec<DungeonTemplate>> {
    let rows = sqlx::query_as::<_, DungeonTemplate>(
        r#"
        SELECT DISTINCT ON (name)
            id, guild_id, name, display_name, emoji, color,
            message_title, message_description, thumbnail_url, image_url,
            requires_vc, showcase_emoji, created_at
        FROM dungeon_templates
        WHERE guild_id = $1 OR guild_id IS NULL
        ORDER BY name, guild_id NULLS LAST
        "#,
    )
    .bind(guild_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Guild-specific template preferred; falls back to global.
pub async fn get_by_name(
    pool: &PgPool,
    guild_id: i64,
    name: &str,
) -> Result<Option<DungeonTemplate>> {
    let row = sqlx::query_as::<_, DungeonTemplate>(
        r#"
        SELECT id, guild_id, name, display_name, emoji, color,
               message_title, message_description, thumbnail_url, image_url,
               requires_vc, showcase_emoji, created_at
        FROM dungeon_templates
        WHERE name = $1 AND (guild_id = $2 OR guild_id IS NULL)
        ORDER BY guild_id NULLS LAST
        LIMIT 1
        "#,
    )
    .bind(name)
    .bind(guild_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Fetch a template by its primary key (used by component handlers).
pub async fn get_by_id(pool: &PgPool, id: i32) -> Result<Option<DungeonTemplate>> {
    let row = sqlx::query_as::<_, DungeonTemplate>(
        r#"
        SELECT id, guild_id, name, display_name, emoji, color,
               message_title, message_description, thumbnail_url, image_url,
               requires_vc, showcase_emoji, created_at
        FROM dungeon_templates WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Reactions for a template, ordered by sort_order.
pub async fn get_reactions(pool: &PgPool, template_id: i32) -> Result<Vec<DungeonReaction>> {
    let rows = sqlx::query_as::<_, DungeonReaction>(
        r#"
        SELECT id, dungeon_template_id, name, display_name, emoji,
               num_required, requires_confirmation, sort_order
        FROM dungeon_reactions
        WHERE dungeon_template_id = $1
        ORDER BY sort_order
        "#,
    )
    .bind(template_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub struct NewTemplate<'a> {
    pub name: &'a str,
    pub display_name: &'a str,
    pub emoji: Option<&'a str>,
    pub color: Option<i32>,
    pub message_title: Option<&'a str>,
    pub message_description: Option<&'a str>,
    pub requires_vc: bool,
}

/// Create a guild-specific template override.
pub async fn insert_guild_template(
    pool: &PgPool,
    guild_id: i64,
    t: &NewTemplate<'_>,
) -> Result<i32> {
    let id: i32 = sqlx::query_scalar(
        r#"
        INSERT INTO dungeon_templates
            (guild_id, name, display_name, emoji, color,
             message_title, message_description, requires_vc)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        RETURNING id
        "#,
    )
    .bind(guild_id)
    .bind(t.name)
    .bind(t.display_name)
    .bind(t.emoji)
    .bind(t.color)
    .bind(t.message_title)
    .bind(t.message_description)
    .bind(t.requires_vc)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Update a guild-specific template; returns false if the row doesn't belong to this guild.
// Phase D will lift the patch fields into a dedicated `TemplatePatch` struct
// alongside the snowflake-newtype migration.
#[allow(clippy::too_many_arguments)]
pub async fn update_guild_template(
    pool: &PgPool,
    guild_id: i64,
    name: &str,
    display_name: Option<&str>,
    emoji: Option<&str>,
    color: Option<i32>,
    message_title: Option<&str>,
    message_description: Option<&str>,
    requires_vc: Option<bool>,
) -> Result<bool> {
    let rows = sqlx::query(
        r#"
        UPDATE dungeon_templates SET
            display_name        = COALESCE($3, display_name),
            emoji               = COALESCE($4, emoji),
            color               = COALESCE($5, color),
            message_title       = COALESCE($6, message_title),
            message_description = COALESCE($7, message_description),
            requires_vc         = COALESCE($8, requires_vc)
        WHERE name = $1 AND guild_id = $2
        "#,
    )
    .bind(name)
    .bind(guild_id)
    .bind(display_name)
    .bind(emoji)
    .bind(color)
    .bind(message_title)
    .bind(message_description)
    .bind(requires_vc)
    .execute(pool)
    .await?;
    Ok(rows.rows_affected() > 0)
}

/// Delete a guild-specific template; returns false if not found or is global.
pub async fn delete_guild_template(pool: &PgPool, guild_id: i64, name: &str) -> Result<bool> {
    let rows = sqlx::query("DELETE FROM dungeon_templates WHERE name = $1 AND guild_id = $2")
        .bind(name)
        .bind(guild_id)
        .execute(pool)
        .await?;
    Ok(rows.rows_affected() > 0)
}

/// Upsert a global template by name (called by the templates-module seeder
/// on bot boot). Authoritative: every field is replaced on conflict, so
/// deleting a value from overrides.json (e.g. clearing a custom
/// `message_title`) propagates back to the DB on restart.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_global_template(
    pool: &PgPool,
    name: &str,
    display_name: &str,
    emoji: Option<&str>,
    color: Option<i32>,
    message_title: Option<&str>,
    message_description: Option<&str>,
    requires_vc: bool,
    showcase_emoji: &[String],
    thumbnail_url: Option<&str>,
) -> Result<i32> {
    let id: i32 = sqlx::query_scalar(
        r#"
        INSERT INTO dungeon_templates
            (guild_id, name, display_name, emoji, color,
             message_title, message_description, requires_vc,
             showcase_emoji, thumbnail_url)
        VALUES (NULL, $1, $2, $3, $4, $5, $6, $7, $8, $9)
        ON CONFLICT ((COALESCE(guild_id, 0)), name)
        DO UPDATE SET
            display_name        = EXCLUDED.display_name,
            emoji               = EXCLUDED.emoji,
            color               = EXCLUDED.color,
            message_title       = EXCLUDED.message_title,
            message_description = EXCLUDED.message_description,
            requires_vc         = EXCLUDED.requires_vc,
            showcase_emoji      = EXCLUDED.showcase_emoji,
            thumbnail_url       = EXCLUDED.thumbnail_url
        RETURNING id
        "#,
    )
    .bind(name)
    .bind(display_name)
    .bind(emoji)
    .bind(color)
    .bind(message_title)
    .bind(message_description)
    .bind(requires_vc)
    .bind(showcase_emoji)
    .bind(thumbnail_url)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Names of every global dungeon template (`guild_id IS NULL`).
pub async fn list_global_names(pool: &PgPool) -> Result<Vec<String>> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT name FROM dungeon_templates WHERE guild_id IS NULL")
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|(n,)| n).collect())
}

/// Delete a global (`guild_id IS NULL`) template by name. Returns true iff
/// a row was removed. The FK from live headcounts/runs is the only thing
/// that can block this now — and that means the dungeon is *actively in
/// use*, so the seeder should back off and retry on the next boot.
pub async fn delete_global_by_name(pool: &PgPool, name: &str) -> Result<bool> {
    let rows = sqlx::query("DELETE FROM dungeon_templates WHERE guild_id IS NULL AND name = $1")
        .bind(name)
        .execute(pool)
        .await?;
    Ok(rows.rows_affected() > 0)
}

/// Delete reactions on `template_id` whose `name` is not in `keep_names`.
/// Called after upserting the desired reaction set so stale rows from a
/// previous seed (e.g. sync-wiki's `key` reaction on a dungeon whose
/// override now specifies `lost_halls_key` instead) don't linger.
pub async fn delete_reactions_not_in(
    pool: &PgPool,
    template_id: i32,
    keep_names: &[String],
) -> Result<()> {
    sqlx::query(
        "DELETE FROM dungeon_reactions
         WHERE dungeon_template_id = $1 AND NOT (name = ANY($2))",
    )
    .bind(template_id)
    .bind(keep_names)
    .execute(pool)
    .await?;
    Ok(())
}

/// Bind (or clear) a notification role for a dungeon in a specific guild.
///
/// Writes to `dungeon_notification_roles` keyed by (guild_id, dungeon name)
/// so the binding is decoupled from the template row. No cloning, no
/// shadowing — globals stay authoritative for reactions and display.
pub async fn set_notification_role(
    pool: &PgPool,
    guild_id: i64,
    dungeon_name: &str,
    role_id: Option<i64>,
) -> Result<()> {
    match role_id {
        Some(role) => {
            sqlx::query(
                r#"
                INSERT INTO dungeon_notification_roles (guild_id, dungeon_name, role_id)
                VALUES ($1, $2, $3)
                ON CONFLICT (guild_id, dungeon_name)
                DO UPDATE SET role_id = EXCLUDED.role_id
                "#,
            )
            .bind(guild_id)
            .bind(dungeon_name)
            .bind(role)
            .execute(pool)
            .await?;
        }
        None => {
            sqlx::query(
                "DELETE FROM dungeon_notification_roles WHERE guild_id = $1 AND dungeon_name = $2",
            )
            .bind(guild_id)
            .bind(dungeon_name)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

/// Look up the notification role ID bound to a dungeon in this guild, if any.
pub async fn get_notification_role(
    pool: &PgPool,
    guild_id: i64,
    dungeon_name: &str,
) -> Result<Option<i64>> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT role_id FROM dungeon_notification_roles WHERE guild_id = $1 AND dungeon_name = $2",
    )
    .bind(guild_id)
    .bind(dungeon_name)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(r,)| r))
}

/// Every (dungeon_name → role_id) binding in this guild. Used by the
/// `/pingroles` self-service picker to render the subscription list.
pub async fn list_notification_roles(
    pool: &PgPool,
    guild_id: i64,
) -> Result<std::collections::HashMap<String, i64>> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT dungeon_name, role_id FROM dungeon_notification_roles WHERE guild_id = $1",
    )
    .bind(guild_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Upsert a reaction for a template (used by sync-wiki).
// Phase D will introduce a `ReactionRow` parameter struct.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_reaction(
    pool: &PgPool,
    template_id: i32,
    name: &str,
    display_name: &str,
    emoji: &str,
    num_required: i32,
    requires_confirmation: bool,
    sort_order: i32,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO dungeon_reactions
            (dungeon_template_id, name, display_name, emoji,
             num_required, requires_confirmation, sort_order)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (dungeon_template_id, name)
        DO UPDATE SET
            display_name          = EXCLUDED.display_name,
            emoji                 = EXCLUDED.emoji,
            num_required          = EXCLUDED.num_required,
            requires_confirmation = EXCLUDED.requires_confirmation,
            sort_order            = EXCLUDED.sort_order
        "#,
    )
    .bind(template_id)
    .bind(name)
    .bind(display_name)
    .bind(emoji)
    .bind(num_required)
    .bind(requires_confirmation)
    .bind(sort_order)
    .execute(pool)
    .await?;
    Ok(())
}
