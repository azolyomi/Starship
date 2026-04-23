use anyhow::Result;
use sqlx::PgPool;

use crate::db::models::BotEmoji;

pub async fn upsert(
    pool: &PgPool,
    logical_name: &str,
    discord_emoji_id: i64,
    source_guild_id: Option<i64>,
    category: Option<&str>,
    realmeye_url: Option<&str>,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO bot_emoji (logical_name, discord_emoji_id, source_guild_id, category, realmeye_url)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (logical_name)
        DO UPDATE SET
            discord_emoji_id = EXCLUDED.discord_emoji_id,
            source_guild_id  = COALESCE(EXCLUDED.source_guild_id, bot_emoji.source_guild_id),
            category         = COALESCE(EXCLUDED.category, bot_emoji.category),
            realmeye_url     = COALESCE(EXCLUDED.realmeye_url, bot_emoji.realmeye_url)
        "#,
    )
    .bind(logical_name)
    .bind(discord_emoji_id)
    .bind(source_guild_id)
    .bind(category)
    .bind(realmeye_url)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_by_logical_name(pool: &PgPool, name: &str) -> Result<Option<BotEmoji>> {
    let row = sqlx::query_as::<_, BotEmoji>(
        r#"
        SELECT id, logical_name, discord_emoji_id, source_guild_id, category, realmeye_url
        FROM bot_emoji WHERE logical_name = $1
        "#,
    )
    .bind(name)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn get_all(pool: &PgPool) -> Result<Vec<BotEmoji>> {
    let rows = sqlx::query_as::<_, BotEmoji>(
        r#"
        SELECT id, logical_name, discord_emoji_id, source_guild_id, category, realmeye_url
        FROM bot_emoji ORDER BY logical_name
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn register_emoji_server(
    pool: &PgPool,
    guild_id: i64,
    description: Option<&str>,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO emoji_servers (guild_id, description)
        VALUES ($1, $2)
        ON CONFLICT (guild_id) DO UPDATE SET description = COALESCE(EXCLUDED.description, emoji_servers.description)
        "#,
    )
    .bind(guild_id)
    .bind(description)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_emoji_servers(pool: &PgPool) -> Result<Vec<(i64, Option<String>)>> {
    let rows: Vec<(i64, Option<String>)> =
        sqlx::query_as("SELECT guild_id, description FROM emoji_servers ORDER BY guild_id")
            .fetch_all(pool)
            .await?;
    Ok(rows)
}
