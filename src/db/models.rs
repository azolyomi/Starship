use chrono::{DateTime, Utc};

#[derive(Debug, sqlx::FromRow)]
pub struct Guild {
    pub guild_id: i64,
    pub log_channel_id: Option<i64>,
    pub notification_channel_id: Option<i64>,
    pub superadmin_user_id: Option<i64>,
    pub setup_complete: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Tier {
    pub id: i32,
    pub guild_id: i64,
    pub name: String,
    pub description: Option<String>,
    pub raid_channel_id: Option<i64>,
    pub headcount_channel_id: Option<i64>,
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
    pub notification_role_id: Option<i64>,
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

#[derive(Debug, sqlx::FromRow)]
pub struct Headcount {
    pub id: i32,
    pub guild_id: i64,
    pub tier_id: i32,
    pub dungeon_template_id: i32,
    pub channel_id: i64,
    pub message_id: i64,
    pub leader_user_id: i64,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct HeadcountReaction {
    pub id: i32,
    pub headcount_id: i32,
    pub dungeon_reaction_id: i32,
    pub user_id: i64,
    pub confirmed: bool,
    pub confirmed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct Run {
    pub id: i32,
    pub guild_id: i64,
    pub tier_id: i32,
    pub dungeon_template_id: i32,
    pub headcount_id: Option<i32>,
    pub channel_id: i64,
    pub message_id: i64,
    pub leader_user_id: i64,
    pub location: Option<String>,
    pub party: Option<String>,
    pub voice_channel_id: Option<i64>,
    pub is_vc_raid: bool,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
}
