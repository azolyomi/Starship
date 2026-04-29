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

/// Guild-specific template preferred; falls back to global. Matches against
/// the canonical slug (`name`) first, then the human-readable `display_name`
/// case-insensitively — so users who pick an autocomplete suggestion (slug
/// value) AND users who type "Oryx's Sanctuary" by hand both resolve.
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
        WHERE (name = $1 OR LOWER(display_name) = LOWER($1))
          AND (guild_id = $2 OR guild_id IS NULL)
        ORDER BY (name = $1) DESC, guild_id NULLS LAST
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

/// Parameters for [`create_guild_template_with_inherit`]. Optional
/// fields are user-supplied modal inputs; when an inherit-from source
/// is set, the source's value is the fallback for fields the user didn't
/// provide. Fields that aren't user-editable in the create modal
/// (`emoji`, `requires_vc`, `thumbnail_url`, `showcase_emoji`,
/// `message_title`) always come from the source when present.
pub struct CreateGuildTemplateParams<'a> {
    pub guild_id: i64,
    pub name: &'a str,
    pub display_name: &'a str,
    pub description: Option<&'a str>,
    pub color: Option<i32>,
    pub inherit_from: Option<i32>,
}

/// INSERT a new guild-specific dungeon template, optionally seeded from
/// an inherit-from source. Atomically copies the source's reactions
/// when one is set. Returns the new template id.
pub async fn create_guild_template_with_inherit(
    pool: &PgPool,
    params: CreateGuildTemplateParams<'_>,
) -> Result<i32> {
    let mut tx = pool.begin().await?;

    let new_id: i32 = match params.inherit_from {
        Some(source_id) => {
            // INSERT-SELECT from the source so we pick up `emoji`,
            // `requires_vc`, `message_title`, `thumbnail_url`,
            // `showcase_emoji`, and `image_url` without an extra round
            // trip. User-supplied fields override their counterparts;
            // `description` and `color` fall through to source values
            // when None.
            sqlx::query_scalar(
                r#"
                INSERT INTO dungeon_templates
                    (guild_id, name, display_name, emoji, color,
                     message_title, message_description, requires_vc,
                     showcase_emoji, thumbnail_url, image_url)
                SELECT $1, $2, $3, emoji,
                       COALESCE($4, color),
                       message_title,
                       COALESCE($5, message_description),
                       requires_vc,
                       showcase_emoji, thumbnail_url, image_url
                FROM dungeon_templates
                WHERE id = $6
                RETURNING id
                "#,
            )
            .bind(params.guild_id)
            .bind(params.name)
            .bind(params.display_name)
            .bind(params.color)
            .bind(params.description)
            .bind(source_id)
            .fetch_one(&mut *tx)
            .await?
        }
        None => sqlx::query_scalar(
            r#"
            INSERT INTO dungeon_templates
                (guild_id, name, display_name, color,
                 message_description, requires_vc)
            VALUES ($1, $2, $3, $4, $5, FALSE)
            RETURNING id
            "#,
        )
        .bind(params.guild_id)
        .bind(params.name)
        .bind(params.display_name)
        .bind(params.color)
        .bind(params.description)
        .fetch_one(&mut *tx)
        .await?,
    };

    if let Some(source_id) = params.inherit_from {
        sqlx::query(
            r#"
            INSERT INTO dungeon_reactions
                (dungeon_template_id, name, display_name, emoji,
                 num_required, requires_confirmation, sort_order)
            SELECT $1, name, display_name, emoji,
                   num_required, requires_confirmation, sort_order
            FROM dungeon_reactions
            WHERE dungeon_template_id = $2
            "#,
        )
        .bind(new_id)
        .bind(source_id)
        .execute(&mut *tx)
        .await?;
    }

    // Always-on interest reaction. ON CONFLICT preserves any inherited
    // tuning when the source template already had one (which globals do
    // by default).
    sqlx::query(
        r#"
        INSERT INTO dungeon_reactions
            (dungeon_template_id, name, display_name, emoji,
             num_required, requires_confirmation, sort_order)
        VALUES ($1, 'interest', 'Joining', '✅', 1, FALSE, 0)
        ON CONFLICT (dungeon_template_id, name) DO NOTHING
        "#,
    )
    .bind(new_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(new_id)
}

/// Count of guild-specific templates for a guild. Used by `/dungeon
/// create` to enforce the per-guild cap (`limits::CUSTOM_DUNGEONS_PER_GUILD`)
/// without first listing every row.
pub async fn count_guild_templates(pool: &PgPool, guild_id: i64) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM dungeon_templates WHERE guild_id = $1",
    )
    .bind(guild_id)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

// Used by /dungeon edit in the follow-up commit.
#[allow(dead_code)]
/// Fork a global template into a guild-specific copy, atomically.
///
/// INSERT-SELECT a new `dungeon_templates` row with `guild_id` swapped
/// from NULL to `target_guild_id`, then copy every `dungeon_reactions`
/// row for the source under the new template id. Returns the new
/// template id.
///
/// Used by `/dungeon edit` when an admin tries to edit a global: globals
/// are seed-managed (overrides.json) and shouldn't be mutated per-guild.
/// Forking gives the guild a private copy they can tune freely while the
/// original global stays canonical.
pub async fn clone_global_to_guild(
    pool: &PgPool,
    source_id: i32,
    target_guild_id: i64,
) -> Result<i32> {
    let mut tx = pool.begin().await?;

    let new_id: i32 = sqlx::query_scalar(
        r#"
        INSERT INTO dungeon_templates
            (guild_id, name, display_name, emoji, color,
             message_title, message_description, requires_vc,
             showcase_emoji, thumbnail_url, image_url)
        SELECT $1, name, display_name, emoji, color,
               message_title, message_description, requires_vc,
               showcase_emoji, thumbnail_url, image_url
        FROM dungeon_templates
        WHERE id = $2
        RETURNING id
        "#,
    )
    .bind(target_guild_id)
    .bind(source_id)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO dungeon_reactions
            (dungeon_template_id, name, display_name, emoji,
             num_required, requires_confirmation, sort_order)
        SELECT $1, name, display_name, emoji,
               num_required, requires_confirmation, sort_order
        FROM dungeon_reactions
        WHERE dungeon_template_id = $2
        "#,
    )
    .bind(new_id)
    .bind(source_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(new_id)
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
            // Upsert role; preserve any existing ping_here override.
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
            // Clearing the role: NULL the role_id so the row sticks around
            // if there's a ping_here override to remember; then delete the
            // row if every column has reverted to its default. Two queries
            // are simpler than a CTE here and the ordering is harmless —
            // worst case the DELETE is a no-op.
            sqlx::query(
                r#"
                UPDATE dungeon_notification_roles
                SET role_id = NULL
                WHERE guild_id = $1 AND dungeon_name = $2
                "#,
            )
            .bind(guild_id)
            .bind(dungeon_name)
            .execute(pool)
            .await?;
            sqlx::query(
                r#"
                DELETE FROM dungeon_notification_roles
                WHERE guild_id = $1 AND dungeon_name = $2
                  AND role_id IS NULL AND ping_here = TRUE
                "#,
            )
            .bind(guild_id)
            .bind(dungeon_name)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

/// Set the per-dungeon `@here` ping toggle. Default (no row, or
/// `ping_here = TRUE`) is to ping `@here` alongside the notification
/// role. Setting `false` suppresses the `@here` for this dungeon only.
pub async fn set_ping_here(
    pool: &PgPool,
    guild_id: i64,
    dungeon_name: &str,
    ping_here: bool,
) -> Result<()> {
    // Upsert ping_here; preserve any existing role binding.
    sqlx::query(
        r#"
        INSERT INTO dungeon_notification_roles (guild_id, dungeon_name, ping_here)
        VALUES ($1, $2, $3)
        ON CONFLICT (guild_id, dungeon_name)
        DO UPDATE SET ping_here = EXCLUDED.ping_here
        "#,
    )
    .bind(guild_id)
    .bind(dungeon_name)
    .bind(ping_here)
    .execute(pool)
    .await?;
    // If we're back to all-defaults (no role, ping_here=TRUE), drop the
    // row so the table doesn't accumulate dead config.
    sqlx::query(
        r#"
        DELETE FROM dungeon_notification_roles
        WHERE guild_id = $1 AND dungeon_name = $2
          AND role_id IS NULL AND ping_here = TRUE
        "#,
    )
    .bind(guild_id)
    .bind(dungeon_name)
    .execute(pool)
    .await?;
    Ok(())
}

/// Look up the notification settings for a dungeon: `(role_id, ping_here)`.
/// Default when no row exists is `(None, true)` — no role bound, `@here`
/// pinged on every raid.
pub async fn get_notification_settings(
    pool: &PgPool,
    guild_id: i64,
    dungeon_name: &str,
) -> Result<(Option<i64>, bool)> {
    let row: Option<(Option<i64>, bool)> = sqlx::query_as(
        r#"
        SELECT role_id, ping_here
        FROM dungeon_notification_roles
        WHERE guild_id = $1 AND dungeon_name = $2
        "#,
    )
    .bind(guild_id)
    .bind(dungeon_name)
    .fetch_optional(pool)
    .await?;
    Ok(row.unwrap_or((None, true)))
}

/// Every (dungeon_name → role_id) binding in this guild. Used by the
/// `/pingroles` self-service picker to render the subscription list.
/// Rows that exist only to remember a `ping_here` override (with
/// `role_id IS NULL`) are filtered out — there's nothing for users to
/// subscribe to.
pub async fn list_notification_roles(
    pool: &PgPool,
    guild_id: i64,
) -> Result<std::collections::HashMap<String, i64>> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        r#"
        SELECT dungeon_name, role_id
        FROM dungeon_notification_roles
        WHERE guild_id = $1 AND role_id IS NOT NULL
        "#,
    )
    .bind(guild_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Fetch a single reaction by primary key. Used by the per-reaction
/// tuning modal to pre-fill current values.
pub async fn get_reaction(pool: &PgPool, reaction_id: i32) -> Result<Option<DungeonReaction>> {
    let row = sqlx::query_as::<_, DungeonReaction>(
        r#"
        SELECT id, dungeon_template_id, name, display_name, emoji,
               num_required, requires_confirmation, sort_order
        FROM dungeon_reactions WHERE id = $1
        "#,
    )
    .bind(reaction_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Apply a tuning update to a reaction. The caller is expected to have
/// already validated lengths against `limits::REACTION_DISPLAY_NAME_MAX`.
pub async fn update_reaction(
    pool: &PgPool,
    reaction_id: i32,
    display_name: &str,
    num_required: i32,
    sort_order: i32,
    requires_confirmation: bool,
) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE dungeon_reactions SET
            display_name          = $2,
            num_required          = $3,
            sort_order            = $4,
            requires_confirmation = $5
        WHERE id = $1
        "#,
    )
    .bind(reaction_id)
    .bind(display_name)
    .bind(num_required)
    .bind(sort_order)
    .bind(requires_confirmation)
    .execute(pool)
    .await?;
    Ok(())
}

/// Delete a single reaction from a template by its logical name.
/// Returns true iff a row was deleted. Used by `/dungeon edit`'s
/// per-category multi-select when an admin deselects a reaction.
pub async fn delete_reaction_by_name(
    pool: &PgPool,
    template_id: i32,
    name: &str,
) -> Result<bool> {
    let result = sqlx::query(
        "DELETE FROM dungeon_reactions WHERE dungeon_template_id = $1 AND name = $2",
    )
    .bind(template_id)
    .bind(name)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
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
