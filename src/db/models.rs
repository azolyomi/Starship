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
    /// Discord role granted on successful verification. `None` until
    /// `/setup`'s verification section is configured.
    pub verified_role_id: Option<i64>,
    /// Channel hosting the persistent Verify button message. `None` until
    /// configured.
    pub verify_channel_id: Option<i64>,
    /// Message ID of the persistent Verify button. `None` if never posted
    /// or if the startup sweep cleared it after a 404.
    pub verify_message_id: Option<i64>,
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
    /// Per-tier opt-in for the start-run UI: a sticky "Start a run" button
    /// plus an auto-updating active-raids listing in the configured
    /// channel. The headcount protection gates (slot lock, per-user cap,
    /// post-cancel cooldown, min-reactor convert gate) are universal and
    /// apply regardless of this flag.
    pub enable_start_run_ui: bool,
    /// Channel hosting the sticky "Start a run" button + active-raids
    /// listing. Only meaningful when `enable_start_run_ui` is true.
    pub start_run_ui_channel_id: Option<i64>,
    /// Message ID of the sticky button. `None` until installed; reposted
    /// when 404'd.
    pub start_run_ui_button_message_id: Option<i64>,
    /// Message ID of the sticky active-raids listing. Edited in-place on
    /// every state transition.
    pub start_run_ui_listing_message_id: Option<i64>,
    /// HC age beyond which the slot is considered stale and may be
    /// displaced by a new headcount start for the same dungeon.
    pub hc_idle_minutes: i32,
    /// Cooldown applied after a leader cancels their own headcount,
    /// blocking another start in the same tier. Bypassed for organizers.
    pub hc_cancel_cooldown_seconds: i32,
    /// Minimum distinct non-bot reactors required for HC->Run conversion.
    /// Bypassed for organizers (admins / ManageRuns).
    pub hc_min_reactors: i32,
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

#[derive(Debug, Clone, sqlx::FromRow)]
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

/// A pending verification attempt. Lives only between the user submitting
/// their IGN and the user clicking "I added it" (or `expires_at` elapsing).
/// PK on (guild_id, discord_user_id) — re-running /verify silently
/// overwrites an in-flight attempt for the same user.
#[derive(Debug, sqlx::FromRow)]
pub struct PendingVerification {
    pub guild_id: i64,
    pub discord_user_id: i64,
    pub claimed_ign: String,
    pub code: String,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

/// A completed verification: a Discord user is bound to a RealmEye IGN
/// within one guild. Rebind = overwrite by PK. UNIQUE (guild_id,
/// realmeye_ign) blocks two Discord users from claiming the same IGN.
#[derive(Debug, sqlx::FromRow)]
pub struct VerifiedUser {
    pub guild_id: i64,
    pub discord_user_id: i64,
    pub realmeye_ign: String,
    pub verified_at: DateTime<Utc>,
    /// `None` for self-verifies; admin's user_id when verified via /mv.
    pub verified_by: Option<i64>,
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
