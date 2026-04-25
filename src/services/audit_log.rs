//! Best-effort guild-side audit log posting.
//!
//! Each guild can configure a log channel via `/setup` or `/config
//! log-channel`. When set, structured events (verifications today; more
//! to follow) post a one-line summary there. When unset, this module
//! silently no-ops — there's no other side effect.
//!
//! Failures (kicked from the channel, channel deleted, Discord blip)
//! are logged as `warn!` and swallowed. The audit log is informational,
//! never load-bearing — a missed entry never blocks the user-facing
//! action that triggered it.

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, CreateMessage, Http};
use sqlx::PgPool;
use tracing::warn;

use crate::db;

/// Post `content` to the guild's configured log channel, if any. Errors
/// (DB read, Discord send) are logged and swallowed — this never bubbles
/// out, so callers can fire-and-forget without wrapping in their own
/// `if let Err(_)`.
pub async fn post(http: &Http, pool: &PgPool, guild_id: i64, content: impl Into<String>) {
    let content = content.into();

    let guild = match db::guild::get(pool, guild_id).await {
        Ok(Some(g)) => g,
        Ok(None) => return,
        Err(e) => {
            warn!(error = ?e, guild_id, "audit_log: failed to read guild row");
            return;
        }
    };

    let Some(log_channel_id) = guild.log_channel_id else {
        return;
    };

    let channel = ChannelId::new(log_channel_id as u64);
    if let Err(e) = channel
        .send_message(http, CreateMessage::new().content(&content))
        .await
    {
        warn!(
            error = ?e,
            guild_id,
            log_channel_id,
            content = %content,
            "audit_log: failed to post message",
        );
    }
}
