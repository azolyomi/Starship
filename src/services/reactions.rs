//! Attach native Discord reactions to a freshly-posted headcount / run
//! message so users can sign up by clicking them directly. R4 replaced the
//! old DB-tracked button flow with this — no per-user state.
//!
//! The bot fires one `POST /channels/{id}/messages/{id}/reactions/{emoji}/@me`
//! per required reaction. Each call is retried up to 5 times on 429 or 5xx
//! with exponential backoff. If any reaction still fails, we @-mention the
//! organizer in the same channel so they know the message is missing a
//! reaction — better a loud failure than a silent one a raider notices
//! mid-raid.

use std::time::Duration;

use anyhow::Result;
use poise::serenity_prelude as serenity;
use serenity::{ChannelId, Http, MessageId, ReactionType};
use tokio::time::sleep;
use tracing::warn;

const MAX_RETRIES: usize = 5;
const BASE_BACKOFF_MS: u64 = 250;

/// Attach `reactions` to `(channel_id, message_id)` in order. Returns the
/// list of reactions that failed after MAX_RETRIES; empty slice = success.
///
/// This function does NOT return an error on partial failure — a missing
/// reaction isn't worth nuking the whole headcount/run for. The caller is
/// expected to pass the result to [`ping_organizer_on_failure`] (or equivalent)
/// so the organizer hears about it.
pub async fn attach_reactions(
    http: &Http,
    channel_id: ChannelId,
    message_id: MessageId,
    reactions: &[ReactionType],
) -> Vec<ReactionType> {
    let mut failures: Vec<ReactionType> = Vec::new();

    for rt in reactions {
        if let Err(e) = try_react(http, channel_id, message_id, rt).await {
            warn!(
                channel_id = %channel_id,
                message_id = %message_id,
                reaction = ?rt,
                error = ?e,
                "giving up on attaching reaction"
            );
            failures.push(rt.clone());
        }
    }

    failures
}

async fn try_react(
    http: &Http,
    channel_id: ChannelId,
    message_id: MessageId,
    rt: &ReactionType,
) -> Result<()> {
    let mut attempt = 0;
    loop {
        match http.create_reaction(channel_id, message_id, rt).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                attempt += 1;
                if attempt >= MAX_RETRIES || !is_retryable(&e) {
                    return Err(e.into());
                }
                let delay = BASE_BACKOFF_MS * (1u64 << (attempt - 1).min(5));
                warn!(
                    attempt,
                    delay_ms = delay,
                    reaction = ?rt,
                    error = ?e,
                    "transient error attaching reaction, retrying"
                );
                sleep(Duration::from_millis(delay)).await;
            }
        }
    }
}

/// 429s and 5xx are transient; everything else (403, 404, bad emoji) is
/// not going to resolve itself on retry.
fn is_retryable(err: &serenity::Error) -> bool {
    match err {
        serenity::Error::Http(http_err) => match http_err {
            serenity::HttpError::RateLimitI64F64 => true,
            serenity::HttpError::UnsuccessfulRequest(resp) => {
                let code = resp.status_code.as_u16();
                code == 429 || (500..=599).contains(&code)
            }
            _ => false,
        },
        _ => false,
    }
}

/// If any reactions failed to attach, @-mention the organizer in the same
/// channel so they can add the missing reactions by hand. Best-effort —
/// errors here are swallowed (the worst case is worse than a silent failure
/// isn't much worse than it, and spamming isn't the goal).
pub async fn ping_organizer_on_failure(
    http: &Http,
    channel_id: ChannelId,
    organizer_id: u64,
    message_id: MessageId,
    failures: &[ReactionType],
) {
    if failures.is_empty() {
        return;
    }
    let list: Vec<String> = failures.iter().map(format_reaction).collect();
    let content = format!(
        "<@{organizer_id}> — couldn't attach {} reaction(s) to [the raid message](https://discord.com/channels/@me/{channel_id}/{message_id}): {}. Please add them manually.",
        failures.len(),
        list.join(" "),
    );
    if let Err(e) = channel_id
        .send_message(http, serenity::CreateMessage::new().content(content))
        .await
    {
        warn!(
            error = ?e,
            channel_id = %channel_id,
            organizer_id,
            "failed to ping organizer about missing reactions",
        );
    }
}

fn format_reaction(rt: &ReactionType) -> String {
    match rt {
        ReactionType::Unicode(s) => s.clone(),
        ReactionType::Custom { id, name, animated } => {
            let name = name.as_deref().unwrap_or("emoji");
            if *animated {
                format!("<a:{name}:{id}>")
            } else {
                format!("<:{name}:{id}>")
            }
        }
        _ => "?".to_string(),
    }
}
