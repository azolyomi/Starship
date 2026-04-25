//! sqlx-hydrated row structs that mirror tables 1:1.
//!
//! Every field is required by `sqlx::FromRow` to populate from the
//! corresponding `SELECT *`-style query, even when no caller currently
//! reads it. Trimming a struct field would force the SQL to drop the
//! column and re-add it the moment a future caller needs it. Mirroring
//! the schema here is the contract — `#[allow(dead_code)]` stays at the
//! module level and individual structs are not re-annotated.

#![allow(dead_code)]

use chrono::{DateTime, Utc};

#[derive(Debug, sqlx::FromRow)]
pub struct Guild {
    pub guild_id: i64,
    pub log_channel_id: Option<i64>,
    pub superadmin_user_id: Option<i64>,
    pub setup_complete: bool,
    pub loot_tier_threshold: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Tier {
    pub id: i32,
    pub guild_id: i64,
    pub name: String,
    pub description: Option<String>,
    /// Where headcount + run messages post. `None` = tier still needs
    /// configuration (`/setup` or `/tier edit`).
    pub runs_channel_id: Option<i64>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct DungeonTemplate {
    pub id: i32,
    pub guild_id: Option<i64>,
    pub name: String,
    pub display_name: String,
    pub emoji: Option<String>,
    pub color: Option<i32>,
    pub message_title: Option<String>,
    pub message_description: Option<String>,
    pub thumbnail_url: Option<String>,
    pub image_url: Option<String>,
    pub requires_vc: bool,
    pub showcase_emoji: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct DungeonReaction {
    pub id: i32,
    pub dungeon_template_id: i32,
    pub name: String,
    pub display_name: String,
    pub emoji: String,
    pub num_required: i32,
    pub requires_confirmation: bool,
    pub sort_order: i32,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BagTier {
    pub name: String,
    pub sort_order: i32,
    pub default_emoji: String,
}

#[derive(Debug, sqlx::FromRow)]
pub struct BotEmoji {
    pub id: i32,
    pub logical_name: String,
    pub discord_emoji_id: i64,
    pub name_on_discord: String,
    pub animated: bool,
    pub source_guild_id: Option<i64>,
    pub category: Option<String>,
    pub realmeye_url: Option<String>,
    pub uploaded_at: DateTime<Utc>,
    pub bag_tier: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct Permission {
    pub id: i32,
    pub guild_id: i64,
    pub role_id: i64,
    pub action: String,
    pub tier_id: Option<i32>,
    pub dungeon_template_id: Option<i32>,
}

/// A live headcount. Rows exist only while the headcount is waiting for a
/// leader to hit Start / Cancel — terminal transitions delete the row.
#[derive(Debug, sqlx::FromRow)]
pub struct Headcount {
    pub id: i32,
    pub guild_id: i64,
    pub tier_id: i32,
    pub dungeon_template_id: i32,
    pub channel_id: i64,
    pub message_id: i64,
    pub leader_user_id: i64,
    pub location: Option<String>,
    pub party: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// A live run. Rows exist only while the run is active — End deletes.
#[derive(Debug, sqlx::FromRow)]
pub struct Run {
    pub id: i32,
    pub guild_id: i64,
    pub tier_id: i32,
    pub dungeon_template_id: i32,
    pub channel_id: i64,
    pub message_id: i64,
    pub leader_user_id: i64,
    pub location: Option<String>,
    pub party: Option<String>,
    pub voice_channel_id: Option<i64>,
    pub is_vc_raid: bool,
    pub created_at: DateTime<Utc>,
}
