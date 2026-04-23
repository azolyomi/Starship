use std::collections::HashMap;

use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::db::models::{Headcount, HeadcountReaction};

pub async fn create(
    pool: &PgPool,
    guild_id: i64,
    tier_id: i32,
    dungeon_template_id: i32,
    channel_id: i64,
    leader_user_id: i64,
) -> Result<Headcount> {
    let row = sqlx::query_as::<_, Headcount>(
        r#"
        INSERT INTO headcounts
            (guild_id, tier_id, dungeon_template_id, channel_id, message_id, leader_user_id)
        VALUES ($1, $2, $3, $4, 0, $5)
        RETURNING *
        "#,
    )
    .bind(guild_id)
    .bind(tier_id)
    .bind(dungeon_template_id)
    .bind(channel_id)
    .bind(leader_user_id)
    .fetch_one(pool)
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

pub async fn get(pool: &PgPool, id: i32) -> Result<Option<Headcount>> {
    let row = sqlx::query_as::<_, Headcount>("SELECT * FROM headcounts WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

pub async fn set_status(pool: &PgPool, id: i32, status: &str) -> Result<()> {
    sqlx::query("UPDATE headcounts SET status = $1 WHERE id = $2")
        .bind(status)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn list_active(pool: &PgPool, guild_id: i64) -> Result<Vec<Headcount>> {
    let rows = sqlx::query_as::<_, Headcount>(
        "SELECT * FROM headcounts WHERE guild_id = $1 AND status = 'active'",
    )
    .bind(guild_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// ---------------------------------------------------------------------------
// Reaction counts
// ---------------------------------------------------------------------------

pub struct ReactionCount {
    pub total: i64,
    pub confirmed: i64,
}

pub async fn reaction_counts(
    pool: &PgPool,
    headcount_id: i32,
) -> Result<HashMap<i32, ReactionCount>> {
    struct Row {
        dungeon_reaction_id: i32,
        total: i64,
        confirmed: i64,
    }

    let rows = sqlx::query_as!(
        Row,
        r#"
        SELECT dungeon_reaction_id,
               COUNT(*)::BIGINT                           AS "total!: i64",
               COUNT(*) FILTER (WHERE confirmed)::BIGINT  AS "confirmed!: i64"
        FROM headcount_reactions
        WHERE headcount_id = $1
        GROUP BY dungeon_reaction_id
        "#,
        headcount_id
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| (r.dungeon_reaction_id, ReactionCount { total: r.total, confirmed: r.confirmed }))
        .collect())
}

// ---------------------------------------------------------------------------
// Per-user reaction manipulation
// ---------------------------------------------------------------------------

pub async fn get_user_reaction(
    pool: &PgPool,
    headcount_id: i32,
    dungeon_reaction_id: i32,
    user_id: i64,
) -> Result<Option<HeadcountReaction>> {
    let row = sqlx::query_as::<_, HeadcountReaction>(
        r#"
        SELECT id, headcount_id, dungeon_reaction_id, user_id, confirmed, confirmed_at
        FROM headcount_reactions
        WHERE headcount_id = $1 AND dungeon_reaction_id = $2 AND user_id = $3
        "#,
    )
    .bind(headcount_id)
    .bind(dungeon_reaction_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn add_reaction(
    pool: &PgPool,
    headcount_id: i32,
    dungeon_reaction_id: i32,
    user_id: i64,
    confirmed: bool,
) -> Result<()> {
    let confirmed_at: Option<DateTime<Utc>> = if confirmed { Some(Utc::now()) } else { None };
    sqlx::query(
        r#"
        INSERT INTO headcount_reactions
            (headcount_id, dungeon_reaction_id, user_id, confirmed, confirmed_at)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (headcount_id, dungeon_reaction_id, user_id) DO UPDATE
            SET confirmed    = EXCLUDED.confirmed,
                confirmed_at = EXCLUDED.confirmed_at
        "#,
    )
    .bind(headcount_id)
    .bind(dungeon_reaction_id)
    .bind(user_id)
    .bind(confirmed)
    .bind(confirmed_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn remove_reaction(
    pool: &PgPool,
    headcount_id: i32,
    dungeon_reaction_id: i32,
    user_id: i64,
) -> Result<()> {
    sqlx::query(
        r#"
        DELETE FROM headcount_reactions
        WHERE headcount_id = $1 AND dungeon_reaction_id = $2 AND user_id = $3
        "#,
    )
    .bind(headcount_id)
    .bind(dungeon_reaction_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}
