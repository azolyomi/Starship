//! Tracing → Discord DM bridge.
//!
//! Captures `WARN`/`ERROR` events from the bot's tracing subscriber and DMs
//! each configured user a 30-second-batched, deduplicated summary. Useful
//! as a cheap "is the bot screaming about anything?" signal without
//! plumbing in a full observability stack.
//!
//! Wiring lives in two places:
//!
//! * [`install`] is called from `main::init_tracing` and returns the
//!   tracing [`Layer`] (which gets composed into the global subscriber)
//!   plus the receiver half of the dispatch channel.
//! * [`spawn_dispatch_loop`] is called from the framework setup callback
//!   in `main::run_bot` once the Discord HTTP client is available; it
//!   takes ownership of the receiver and a list of recipient user IDs.
//!
//! ## Recursion guard
//!
//! Events from `serenity::*` or this module itself are dropped before
//! they enter the channel. Without this, a Discord outage would loop
//! forever — every failed DM attempt would log an error, which the
//! layer would try to DM, etc.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use poise::serenity_prelude as serenity;
use tokio::sync::mpsc;
use tracing::{
    field::{Field, Visit},
    Event, Level, Subscriber,
};
use tracing_subscriber::{layer::Context, Layer};

const CHANNEL_CAPACITY: usize = 256;
const BATCH_WINDOW: Duration = Duration::from_secs(30);
/// Discord's hard cap is 2000 chars per message; leave room for the
/// triple-backtick fences and a small safety margin.
const DISCORD_MESSAGE_LIMIT: usize = 1900;

/// One captured tracing event, on its way to the dispatch loop.
#[derive(Debug, Clone)]
pub struct DmEvent {
    pub level: Level,
    pub target: String,
    pub message: String,
    pub timestamp: DateTime<Utc>,
}

/// Tracing layer that forwards `WARN`+ events into the dispatch channel.
pub struct DmLayer {
    sender: mpsc::Sender<DmEvent>,
}

impl<S> Layer<S> for DmLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let target = metadata.target();
        // Recursion guard — see module docs.
        if target.starts_with("serenity") || target.starts_with("starship::services::error_dm") {
            return;
        }
        // tracing's `Level` `Ord` is reverse-of-severity (TRACE highest,
        // ERROR lowest), so `level <= WARN` is the "WARN-or-more-severe"
        // filter.
        if *metadata.level() > Level::WARN {
            return;
        }
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let dm_event = DmEvent {
            level: *metadata.level(),
            target: target.to_string(),
            message: visitor.into_message(),
            timestamp: Utc::now(),
        };
        // try_send drops on backpressure; we don't block tracing for DMs.
        // A future iteration could surface drop counts in the next batch.
        let _ = self.sender.try_send(dm_event);
    }
}

/// Visitor that flattens an `Event`'s fields into a single human-readable
/// line. The `message` field (if present) is the headline; structured
/// fields like `error=...` and `guild_id=...` are appended in brackets.
#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: Vec<String>,
}

impl MessageVisitor {
    fn into_message(self) -> String {
        match (self.message.is_empty(), self.fields.is_empty()) {
            (true, true) => "(no message)".to_string(),
            (false, true) => self.message,
            (true, false) => self.fields.join(" "),
            (false, false) => format!("{} [{}]", self.message, self.fields.join(" ")),
        }
    }
}

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.push(format!("{}={}", field.name(), value));
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            self.fields.push(format!("{}={:?}", field.name(), value));
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields.push(format!("{}={}", field.name(), value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields.push(format!("{}={}", field.name(), value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields.push(format!("{}={}", field.name(), value));
    }
}

/// Build the layer + receiver pair. The layer goes into the tracing
/// subscriber at startup; the receiver is held until the Discord HTTP
/// client is available and then handed to [`spawn_dispatch_loop`].
pub fn install() -> (DmLayer, mpsc::Receiver<DmEvent>) {
    let (sender, receiver) = mpsc::channel(CHANNEL_CAPACITY);
    (DmLayer { sender }, receiver)
}

/// Spawn the dispatch task. Drains `receiver`, batches events for
/// `BATCH_WINDOW`, dedups by `(level, target, message)`, and DMs each
/// recipient. Returns without spawning if `recipients` is empty — that
/// drops the receiver, the channel closes, and the layer's `try_send`
/// becomes a quiet no-op for the rest of the process.
pub fn spawn_dispatch_loop(
    http: Arc<serenity::Http>,
    recipients: Vec<serenity::UserId>,
    mut receiver: mpsc::Receiver<DmEvent>,
) {
    if recipients.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let mut accumulator: Vec<DmEvent> = Vec::with_capacity(64);
        let mut interval = tokio::time::interval(BATCH_WINDOW);
        // Skip the immediate first tick so a quiet startup doesn't
        // produce an empty flush.
        interval.tick().await;
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if accumulator.is_empty() {
                        continue;
                    }
                    let batch = std::mem::take(&mut accumulator);
                    flush(&http, &recipients, &batch).await;
                }
                event = receiver.recv() => {
                    match event {
                        Some(e) => accumulator.push(e),
                        None => {
                            // The sender half (held by `DmLayer`) was
                            // dropped — process is shutting down. Flush
                            // anything we have and exit the task.
                            if !accumulator.is_empty() {
                                let batch = std::mem::take(&mut accumulator);
                                flush(&http, &recipients, &batch).await;
                            }
                            break;
                        }
                    }
                }
            }
        }
    });
}

async fn flush(http: &serenity::Http, recipients: &[serenity::UserId], events: &[DmEvent]) {
    let body = format_batch(events);
    for chunk in chunked(&body) {
        for &user_id in recipients {
            send_dm(http, user_id, &chunk).await;
        }
    }
}

/// Group events by (level, target, message), preserving first-seen order
/// so the DM reads chronologically rather than alphabetically. Returns
/// the rendered batch as a single string (caller may chunk it for
/// Discord's 2000-char limit).
fn format_batch(events: &[DmEvent]) -> String {
    let mut groups: HashMap<(Level, &str, &str), (DateTime<Utc>, usize)> = HashMap::new();
    let mut order: Vec<(Level, &str, &str)> = Vec::new();
    for e in events {
        let key = (e.level, e.target.as_str(), e.message.as_str());
        groups
            .entry(key)
            .and_modify(|(_, count)| *count += 1)
            .or_insert_with(|| {
                order.push(key);
                (e.timestamp, 1)
            });
    }
    let mut out = String::new();
    for key in order {
        let (ts, count) = groups[&key];
        let (level, target, message) = key;
        let mult = if count > 1 {
            format!(" (x{count})")
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "[{}] {:5} {}{}\n  {}",
            ts.format("%H:%M:%SZ"),
            level,
            target,
            mult,
            message,
        );
    }
    out
}

/// Split a batch body into Discord-sized code blocks, breaking on line
/// boundaries so a single event isn't ripped in half. Each chunk is
/// wrapped in triple backticks for monospaced rendering.
fn chunked(body: &str) -> Vec<String> {
    let trimmed = body.trim_end();
    if trimmed.len() + 8 <= DISCORD_MESSAGE_LIMIT {
        return vec![format!("```\n{trimmed}\n```")];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in trimmed.lines() {
        // 8 = "```\n" + "\n```" overhead.
        if !current.is_empty() && current.len() + line.len() + 1 + 8 > DISCORD_MESSAGE_LIMIT {
            chunks.push(format!("```\n{}\n```", current.trim_end()));
            current.clear();
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.is_empty() {
        chunks.push(format!("```\n{}\n```", current.trim_end()));
    }
    chunks
}

async fn send_dm(http: &serenity::Http, user_id: serenity::UserId, body: &str) {
    // Failures here are intentionally not logged via tracing — that
    // would re-enter this layer (and either get filtered by the
    // recursion guard or loop). Dropping a single failed DM is the
    // right tradeoff for keeping the bot decoupled from Discord
    // health.
    let Ok(channel) = user_id.create_dm_channel(http).await else {
        return;
    };
    let _ = channel
        .send_message(http, serenity::CreateMessage::new().content(body))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evt(level: Level, target: &str, msg: &str, secs: i64) -> DmEvent {
        DmEvent {
            level,
            target: target.to_string(),
            message: msg.to_string(),
            timestamp: DateTime::<Utc>::from_timestamp(secs, 0).unwrap(),
        }
    }

    #[test]
    fn format_batch_dedups_repeats() {
        let events = vec![
            evt(Level::WARN, "starship::raid", "fetch failed", 100),
            evt(Level::WARN, "starship::raid", "fetch failed", 110),
            evt(Level::WARN, "starship::raid", "fetch failed", 120),
            evt(Level::ERROR, "starship::handlers", "modal err", 130),
        ];
        let out = format_batch(&events);
        // Three repeats collapse to one line marked (x3); distinct event
        // gets its own.
        assert!(out.contains("(x3)"));
        assert!(out.contains("modal err"));
        // Repeated event appears exactly once.
        assert_eq!(out.matches("fetch failed").count(), 1);
    }

    #[test]
    fn format_batch_preserves_first_seen_order() {
        let events = vec![
            evt(Level::WARN, "a", "first", 100),
            evt(Level::WARN, "b", "second", 101),
            evt(Level::WARN, "a", "first", 102),
        ];
        let out = format_batch(&events);
        let pos_first = out.find("first").unwrap();
        let pos_second = out.find("second").unwrap();
        assert!(
            pos_first < pos_second,
            "first-seen event should sort before later ones",
        );
    }

    #[test]
    fn chunked_keeps_short_batches_in_one_message() {
        let body = "[12:00:00Z] WARN  starship::raid\n  fetch failed\n";
        let chunks = chunked(body);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].starts_with("```\n"));
        assert!(chunks[0].ends_with("\n```"));
    }

    #[test]
    fn chunked_splits_long_batches_on_line_boundaries() {
        // ~3000 char body = should split into 2+ chunks, never midline.
        let line = "x".repeat(100);
        let body = (0..30)
            .map(|_| line.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = chunked(&body);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(chunk.len() <= DISCORD_MESSAGE_LIMIT);
            // Every chunk has its own fence pair.
            assert!(chunk.starts_with("```\n"));
            assert!(chunk.ends_with("\n```"));
        }
    }
}
