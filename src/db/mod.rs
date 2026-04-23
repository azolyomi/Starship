pub mod dungeon;
pub mod emoji;
pub mod guild;
pub mod headcount;
pub mod models;
pub mod permission;
pub mod tier;

use anyhow::Result;
use sqlx::PgPool;

pub async fn create_pool(database_url: &str) -> Result<PgPool> {
    let pool = PgPool::connect(database_url).await?;
    Ok(pool)
}
