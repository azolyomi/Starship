use anyhow::Result;
use sqlx::PgPool;

use crate::curation::Curation;
use crate::db::models::{DungeonReaction, DungeonTemplate};
use crate::templates::dungeons::{BuiltinTemplate, BUILTIN_TEMPLATES};

/// Seed global dungeon templates from built-in definitions.
/// Uses DO UPDATE with a no-op so RETURNING always yields the row id.
/// Reactions are filtered through `curation` so a reaction the user removed
/// via `starship curate` doesn't get resurrected on the next startup.
pub async fn seed_builtins(pool: &PgPool, curation: &Curation) -> Result<()> {
    seed_templates(pool, BUILTIN_TEMPLATES, curation).await
}

pub async fn seed_templates(
    pool: &PgPool,
    templates: &[BuiltinTemplate],
    curation: &Curation,
) -> Result<()> {
    for t in templates {
        let id: i32 = sqlx::query_scalar(
            r#"
            INSERT INTO dungeon_templates
                (guild_id, name, display_name, emoji, color,
                 message_title, message_description, requires_vc, showcase_emoji)
            VALUES (NULL, $1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT ((COALESCE(guild_id, 0)), name)
            DO UPDATE SET name = EXCLUDED.name
            RETURNING id
            "#,
        )
        .bind(t.name)
        .bind(t.display_name)
        .bind(t.emoji)
        .bind(t.color)
        .bind(t.message_title)
        .bind(t.message_description)
        .bind(t.requires_vc)
        .bind(t.showcase_emoji)
        .fetch_one(pool)
        .await?;

        for r in t.reactions {
            if !curation.should_keep_reaction(t.name, r.name) {
                continue;
            }
            sqlx::query(
                r#"
                INSERT INTO dungeon_reactions
                    (dungeon_template_id, name, display_name, emoji,
                     num_required, requires_confirmation, sort_order)
                VALUES ($1, $2, $3, $4, $5, $6, $7)
                ON CONFLICT (dungeon_template_id, name) DO NOTHING
                "#,
            )
            .bind(id)
            .bind(r.name)
            .bind(r.display_name)
            .bind(r.emoji)
            .bind(r.num_required)
            .bind(r.requires_confirmation)
            .bind(r.sort_order)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

/// All templates visible to a guild: guild-specific rows override global ones by name.
pub async fn list_for_guild(pool: &PgPool, guild_id: i64) -> Result<Vec<DungeonTemplate>> {
    let rows = sqlx::query_as::<_, DungeonTemplate>(
        r#"
        SELECT DISTINCT ON (name)
            id, guild_id, name, display_name, emoji, color,
            message_title, message_description, thumbnail_url, image_url,
            requires_vc, notification_role_id, showcase_emoji, created_at
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
               requires_vc, notification_role_id, showcase_emoji, created_at
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
               requires_vc, notification_role_id, showcase_emoji, created_at
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
    let rows = sqlx::query(
        "DELETE FROM dungeon_templates WHERE name = $1 AND guild_id = $2",
    )
    .bind(name)
    .bind(guild_id)
    .execute(pool)
    .await?;
    Ok(rows.rows_affected() > 0)
}

/// Upsert a global template by name (used by sync-wiki).
pub async fn upsert_global_template(
    pool: &PgPool,
    name: &str,
    display_name: &str,
    emoji: Option<&str>,
    color: Option<i32>,
    requires_vc: bool,
    showcase_emoji: &[String],
    thumbnail_url: Option<&str>,
) -> Result<i32> {
    let id: i32 = sqlx::query_scalar(
        r#"
        INSERT INTO dungeon_templates
            (guild_id, name, display_name, emoji, color, requires_vc,
             showcase_emoji, thumbnail_url)
        VALUES (NULL, $1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT ((COALESCE(guild_id, 0)), name)
        DO UPDATE SET
            display_name  = EXCLUDED.display_name,
            emoji         = COALESCE(EXCLUDED.emoji, dungeon_templates.emoji),
            color         = COALESCE(EXCLUDED.color, dungeon_templates.color),
            requires_vc   = EXCLUDED.requires_vc,
            showcase_emoji = EXCLUDED.showcase_emoji,
            thumbnail_url  = COALESCE(EXCLUDED.thumbnail_url, dungeon_templates.thumbnail_url)
        RETURNING id
        "#,
    )
    .bind(name)
    .bind(display_name)
    .bind(emoji)
    .bind(color)
    .bind(requires_vc)
    .bind(showcase_emoji)
    .bind(thumbnail_url)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Upsert a reaction for a template (used by sync-wiki).
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
