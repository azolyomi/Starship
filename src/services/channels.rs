//! Helpers for "does this Discord channel still exist?" checks.
//!
//! `/headcount` and the convert-headcount-to-run path both write a DB row
//! and then post a message — if the configured runs channel was deleted in
//! Discord between configuration and use, the post 404s and the user gets
//! a generic "Internal error" while a half-created row is left behind for
//! the orphan sweep to clean up. A pre-flight existence check fails fast
//! with a friendly user message instead.

use anyhow::Result;
use poise::serenity_prelude as serenity;
use serenity::{ChannelId, Http};

/// True iff `err` is a Discord 404. Narrow on purpose: transient failures
/// (rate-limit, 5xx, network) must not be mistaken for "resource gone" or
/// destructive cleanup paths will fire on healthy data.
pub fn is_not_found(err: &serenity::Error) -> bool {
    matches!(
        err,
        serenity::Error::Http(serenity::HttpError::UnsuccessfulRequest(resp))
            if resp.status_code.as_u16() == 404
    )
}

/// Resolve `channel_id` against Discord's API. Returns `Ok(true)` when the
/// channel exists and is visible to the bot, `Ok(false)` when Discord
/// answers 404 (channel deleted, or bot lost access — same handling either
/// way: stop, don't post). Other errors bubble for the caller to log.
pub async fn channel_exists(http: &Http, channel_id: ChannelId) -> Result<bool> {
    match channel_id.to_channel(http).await {
        Ok(_) => Ok(true),
        Err(e) if is_not_found(&e) => Ok(false),
        Err(e) => Err(e.into()),
    }
}
