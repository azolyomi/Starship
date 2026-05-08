//! `/sync-wiki` — operator-only Discord command that runs the same
//! RealmEye scraper as the `starship sync-wiki` CLI subcommand. Same
//! authorisation gate as `/upload-emoji` (`GLOBAL_SUPERADMIN_USER_ID`)
//! since both touch bot-wide application emoji state.
//!
//! Failsafes layered on top of [`crate::cli::sync_wiki::run_core`]:
//!   - User-ID gate (Discord `ADMINISTRATOR` is just the first-pass filter).
//!   - Postgres advisory lock — refuses to start a second run while one is
//!     in flight, including a concurrent shell-side `starship sync-wiki`.
//!     The lock is owned by the worker task so it is always released even
//!     if the slash-command handler returns early.
//!   - `dry_run` parameter for previewing without Discord POSTs / DB writes.
//!   - `purge` requires typing the literal string `PURGE` — no buttons,
//!     no implicit confirmation.
//!   - Periodic ephemeral edits show live progress; final edit shows a
//!     success summary or a failure trace.
//!   - Edit failures (likely 15-min interaction-token expiry) are logged
//!     via `tracing::error!` so the existing error-DM layer mirrors them.

use std::time::{Duration, Instant};

use poise::serenity_prelude::CreateEmbed;
use poise::CreateReply;
use sqlx::PgPool;
use tracing::{error, info};

use crate::cli::sync_wiki::{run_core, Progress, ProgressEvent, SyncOptions, SyncSummary};
use crate::services::permission::GLOBAL_SUPERADMIN_USER_ID;
use crate::{BotContext, BotError};

/// Postgres advisory-lock key for sync-wiki runs. Arbitrary fixed value;
/// the only requirement is that no other code path in the bot uses the
/// same key.
const ADVISORY_LOCK_KEY: i64 = 7_777_777_001;

/// How often to re-render the ephemeral progress message during a run.
/// 5s is fast enough to feel responsive without spamming Discord's
/// per-message rate limit (5 edits / 5s).
const PROGRESS_TICK: Duration = Duration::from_secs(5);

/// Literal string the operator must type into the `purge` slash-command
/// parameter to confirm a destructive run. Anything else aborts.
const PURGE_CONFIRM_TOKEN: &str = "PURGE";

/// Operator-only: scrape RealmEye and refresh the bot's emoji set + wiki dump.
#[poise::command(
    slash_command,
    rename = "sync-wiki",
    default_member_permissions = "ADMINISTRATOR"
)]
pub async fn sync_wiki(
    ctx: BotContext<'_>,
    #[description = "Preview only — no Discord POSTs, no DB writes"] dry_run: Option<bool>,
    #[description = "DESTRUCTIVE: type 'PURGE' to wipe all emojis first"] purge: Option<String>,
) -> Result<(), BotError> {
    if ctx.author().id.get() != GLOBAL_SUPERADMIN_USER_ID {
        ctx.send(
            CreateReply::default()
                .content("Not authorized.")
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    }

    let dry_run = dry_run.unwrap_or(false);
    let purge_confirmed = match purge.as_deref() {
        None => false,
        Some(s) if s == PURGE_CONFIRM_TOKEN => true,
        Some(s) => {
            ctx.send(
                CreateReply::default()
                    .content(format!(
                        "The `purge` parameter must be the exact string `PURGE` to confirm. \
                         Got `{s}`. Run aborted."
                    ))
                    .ephemeral(true),
            )
            .await?;
            return Ok(());
        }
    };

    // Acquire the advisory lock before any Discord chatter so a duplicate
    // invocation gets a clean "already running" reply rather than two
    // overlapping ephemeral progress messages.
    let lock = match SyncWikiLock::try_acquire(&ctx.data().db).await {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            ctx.send(
                CreateReply::default()
                    .content(
                        "Another `/sync-wiki` (or shell-side `starship sync-wiki`) is already \
                         running. Wait for it to finish and try again.",
                    )
                    .ephemeral(true),
            )
            .await?;
            return Ok(());
        }
        Err(e) => {
            error!(error = ?e, "sync-wiki: failed to acquire advisory lock");
            ctx.send(
                CreateReply::default()
                    .content(format!("Failed to acquire run lock: `{e:#}`."))
                    .ephemeral(true),
            )
            .await?;
            return Ok(());
        }
    };

    let started_at = Instant::now();
    let mut state = ProgressState::new(dry_run, purge_confirmed);
    let handle = ctx
        .send(embeds_reply(state.build_embeds(started_at, RenderStatus::Running)).ephemeral(true))
        .await?;

    let (progress, mut rx) = Progress::channel();
    let config = ctx.data().config.clone();
    let pool = ctx.data().db.clone();
    let opts = SyncOptions {
        dry_run,
        purge: purge_confirmed,
    };

    // Move the lock into the worker so it releases on task completion
    // even if this handler returns early (Discord interaction abort, edit
    // failure, etc.). Releasing from the handler would leak the lock if
    // the handler exits before the worker does.
    let mut task = tokio::spawn(async move {
        let result = run_core(&config, Some(&pool), opts, &progress).await;
        if let Err(e) = lock.release().await {
            error!(error = ?e, "sync-wiki: failed to release advisory lock");
        }
        result
    });

    let mut tick = tokio::time::interval(PROGRESS_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await; // discard the immediate first fire

    let summary_result = loop {
        tokio::select! {
            biased;
            // Drain a batch of events on each wake so frequent counter
            // bumps coalesce into one render pass.
            Some(event) = rx.recv() => {
                state.apply(event);
                while let Ok(event) = rx.try_recv() {
                    state.apply(event);
                }
            }
            _ = tick.tick() => {
                let reply = embeds_reply(state.build_embeds(started_at, RenderStatus::Running));
                if let Err(e) = handle.edit(ctx, reply).await {
                    // Most likely the 15-min interaction token expired.
                    // Don't abort the run — it's still side-effecting
                    // correctly; we just lose the live UI. error_dm
                    // mirrors this to the operator.
                    error!(error = ?e, "sync-wiki: failed to edit progress message");
                }
            }
            join = &mut task => {
                break match join {
                    Ok(result) => result,
                    Err(e) => Err(anyhow::anyhow!("sync-wiki worker panicked: {e}")),
                };
            }
        }
    };

    // Drain any tail events the worker emitted right before completing
    // so the final render reflects the very last warning, if any.
    while let Ok(event) = rx.try_recv() {
        state.apply(event);
    }

    let final_status = match &summary_result {
        Ok(summary) => RenderStatus::Complete(summary),
        Err(e) => RenderStatus::Failed(e),
    };
    let final_reply = embeds_reply(state.build_embeds(started_at, final_status));
    if let Err(e) = handle.edit(ctx, final_reply).await {
        error!(error = ?e, "sync-wiki: failed to post final summary");
    }

    if let Err(e) = summary_result {
        error!(error = ?e, "sync-wiki: run failed");
    } else {
        info!("sync-wiki: run completed");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Advisory lock guard.
// ---------------------------------------------------------------------------

/// Holds a Postgres session-scoped advisory lock for the duration of a
/// sync run. We release explicitly via [`SyncWikiLock::release`] rather
/// than relying on connection-drop, because sqlx pools may return the
/// connection to the pool with the session still alive — leaking the
/// lock until the next bot restart.
struct SyncWikiLock {
    conn: sqlx::pool::PoolConnection<sqlx::Postgres>,
}

impl SyncWikiLock {
    async fn try_acquire(pool: &PgPool) -> anyhow::Result<Option<Self>> {
        let mut conn = pool.acquire().await?;
        let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(ADVISORY_LOCK_KEY)
            .fetch_one(&mut *conn)
            .await?;
        if acquired {
            Ok(Some(Self { conn }))
        } else {
            Ok(None)
        }
    }

    async fn release(mut self) -> anyhow::Result<()> {
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(ADVISORY_LOCK_KEY)
            .execute(&mut *self.conn)
            .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Progress aggregator + ephemeral embed renderer.
// ---------------------------------------------------------------------------

/// Embed colors. Yellow during a run, green on success, red on failure —
/// the bar down the left of the embed reads the run state at a glance.
const COLOR_RUNNING: u32 = 0xFEE75C;
const COLOR_OK: u32 = 0x57F287;
const COLOR_FAIL: u32 = 0xED4245;

/// Per-message Discord caps (combined across all embeds): 6000 chars,
/// max 10 embeds. We leave headroom for embed titles and small overhead
/// the budget tracker doesn't account for individually.
const MAX_TOTAL_DESC_CHARS: usize = 5500;
const MAX_DESC_CHARS: usize = 3900;
const MAX_EMBEDS: usize = 9;

/// One newly-uploaded emoji, captured from `ProgressEvent::UploadNew` so
/// the renderer can build `<:name:id>` markup directly without
/// re-parsing the event stream.
#[derive(Clone)]
struct AddedEmoji {
    discord_name: String,
    emoji_id: u64,
    animated: bool,
}

#[derive(Default, Clone)]
struct ProgressState {
    dry_run: bool,
    purge: bool,
    phase: String,
    current_dungeon: Option<(String, usize, usize)>,
    uploads_reused: usize,
    uploads_skipped: usize,
    added: Vec<AddedEmoji>,
    warnings_log: Vec<String>,
}

impl ProgressState {
    fn new(dry_run: bool, purge: bool) -> Self {
        Self {
            dry_run,
            purge,
            phase: "starting up".into(),
            ..Self::default()
        }
    }

    fn apply(&mut self, event: ProgressEvent) {
        match event {
            ProgressEvent::Phase(p) => self.phase = p,
            ProgressEvent::DungeonStart { name, index, total } => {
                self.phase = "processing dungeons".into();
                self.current_dungeon = Some((name, index, total));
            }
            ProgressEvent::Warning(msg) => self.warnings_log.push(msg),
            ProgressEvent::UploadNew {
                discord_name,
                emoji_id,
                animated,
            } => self.added.push(AddedEmoji {
                discord_name,
                emoji_id,
                animated,
            }),
            ProgressEvent::UploadReused => self.uploads_reused += 1,
            ProgressEvent::UploadSkipped => self.uploads_skipped += 1,
        }
    }

    fn flags_line(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if self.dry_run {
            parts.push("dry-run");
        }
        if self.purge {
            parts.push("PURGE");
        }
        if parts.is_empty() {
            "live".into()
        } else {
            parts.join(", ")
        }
    }

    /// Build the chain of embeds for the live ephemeral message.
    /// Always includes a status embed; optionally adds one or more
    /// "Added emojis" embeds (each holding a slice of the new emojis as
    /// `<:name:id>` markup) and one or more "Warnings" embeds. When the
    /// per-message char budget is exhausted, a `…and N more` marker is
    /// appended to the last description chunk and any further entries
    /// are dropped (they remain in the journal logs).
    fn build_embeds(&self, started_at: Instant, status: RenderStatus<'_>) -> Vec<CreateEmbed> {
        let mut budget = EmbedBudget::default();
        let mut embeds = Vec::with_capacity(3);

        embeds.push(self.status_embed(started_at, status, &mut budget));

        if !self.added.is_empty() {
            embeds.extend(self.added_embeds(status.color(), &mut budget));
        }

        if !self.warnings_log.is_empty() {
            embeds.extend(self.warnings_embeds(&mut budget));
        }

        embeds
    }

    fn status_embed(
        &self,
        started_at: Instant,
        status: RenderStatus<'_>,
        budget: &mut EmbedBudget,
    ) -> CreateEmbed {
        let elapsed = format_elapsed(started_at.elapsed());
        let title = match status {
            RenderStatus::Running => format!("Running /sync-wiki — {elapsed} elapsed"),
            RenderStatus::Complete(_) => format!("Done — /sync-wiki in {elapsed}"),
            RenderStatus::Failed(_) => format!("Failed — /sync-wiki after {elapsed}"),
        };

        let mut desc = format!("Flags: `{}`\nPhase: {}\n", self.flags_line(), self.phase);
        if let Some((name, idx, total)) = &self.current_dungeon {
            desc.push_str(&format!("Current: {name} ({idx}/{total})\n"));
        }
        if let RenderStatus::Complete(summary) = status {
            desc.push_str(&format!("Dungeons scraped: {}\n", summary.dungeons_scraped));
        }
        desc.push_str(&format!(
            "Uploads: **{}** new, {} reused",
            self.added.len(),
            self.uploads_reused,
        ));
        if self.dry_run {
            desc.push_str(&format!(", {} would-upload", self.uploads_skipped));
        }
        desc.push('\n');
        desc.push_str(&format!("Warnings: **{}**", self.warnings_log.len()));

        if let RenderStatus::Failed(err) = status {
            desc.push_str(&format!(
                "\n\nError: ```\n{}\n```",
                truncate(&format!("{err:#}"), 1500),
            ));
        }

        budget.record(&desc);
        CreateEmbed::new()
            .title(title)
            .description(desc)
            .color(status.color())
    }

    fn added_embeds(&self, color: u32, budget: &mut EmbedBudget) -> Vec<CreateEmbed> {
        let total = self.added.len();
        let mut chunks: Vec<String> = Vec::new();
        let mut idx = 0;

        while idx < total && chunks.len() + 1 < MAX_EMBEDS {
            let mut desc = String::new();
            let entries_start = idx;

            while idx < total {
                let line = format_added_line(&self.added[idx]);
                let line_chars = line.chars().count();
                if desc.chars().count() + line_chars > MAX_DESC_CHARS {
                    break;
                }
                if line_chars > budget.remaining() {
                    break;
                }
                budget.record(&line);
                desc.push_str(&line);
                idx += 1;
            }

            if entries_start == idx {
                break;
            }
            chunks.push(desc);
        }

        if idx < total {
            append_truncation_marker(&mut chunks, total - idx, budget);
        }

        let n = chunks.len();
        chunks
            .into_iter()
            .enumerate()
            .map(|(i, desc)| {
                let title = if n == 1 {
                    format!("Added emojis ({total})")
                } else {
                    format!("Added emojis ({total}) — part {}/{n}", i + 1)
                };
                CreateEmbed::new()
                    .title(title)
                    .description(desc)
                    .color(color)
            })
            .collect()
    }

    fn warnings_embeds(&self, budget: &mut EmbedBudget) -> Vec<CreateEmbed> {
        let total = self.warnings_log.len();
        let mut chunks: Vec<String> = Vec::new();
        let mut idx = 0;

        while idx < total && chunks.len() + 1 < MAX_EMBEDS {
            let mut desc = String::new();
            let entries_start = idx;

            while idx < total {
                let line = format!("- {}\n", truncate(&self.warnings_log[idx], 240));
                let line_chars = line.chars().count();
                if desc.chars().count() + line_chars > MAX_DESC_CHARS {
                    break;
                }
                if line_chars > budget.remaining() {
                    break;
                }
                budget.record(&line);
                desc.push_str(&line);
                idx += 1;
            }

            if entries_start == idx {
                break;
            }
            chunks.push(desc);
        }

        if idx < total {
            append_truncation_marker(&mut chunks, total - idx, budget);
        }

        let n = chunks.len();
        chunks
            .into_iter()
            .enumerate()
            .map(|(i, desc)| {
                let title = if n == 1 {
                    format!("Warnings ({total})")
                } else {
                    format!("Warnings ({total}) — part {}/{n}", i + 1)
                };
                CreateEmbed::new()
                    .title(title)
                    .description(desc)
                    .color(COLOR_FAIL)
            })
            .collect()
    }
}

/// Render one added emoji as a single description line: the emoji
/// itself followed by its slug for at-a-glance identification.
fn format_added_line(emoji: &AddedEmoji) -> String {
    let prefix = if emoji.animated { "a" } else { "" };
    format!(
        "<{prefix}:{name}:{id}> `{name}`\n",
        name = emoji.discord_name,
        id = emoji.emoji_id,
    )
}

fn append_truncation_marker(chunks: &mut [String], remaining: usize, budget: &mut EmbedBudget) {
    let marker = format!("\n_…and {remaining} more — see journal logs_");
    let chars = marker.chars().count();
    if let Some(last) = chunks.last_mut() {
        if budget.remaining() >= chars {
            budget.record(&marker);
            last.push_str(&marker);
        }
    }
}

#[derive(Copy, Clone)]
enum RenderStatus<'a> {
    Running,
    Complete(&'a SyncSummary),
    Failed(&'a anyhow::Error),
}

impl<'a> RenderStatus<'a> {
    fn color(self) -> u32 {
        match self {
            RenderStatus::Running => COLOR_RUNNING,
            RenderStatus::Complete(_) => COLOR_OK,
            RenderStatus::Failed(_) => COLOR_FAIL,
        }
    }
}

/// Tracks total characters spent across all embed descriptions for the
/// live message. We don't account for titles separately — the headroom
/// between [`MAX_TOTAL_DESC_CHARS`] and Discord's actual 6000 cap covers
/// the ~9 × ~50-char titles plus padding.
#[derive(Default)]
struct EmbedBudget {
    used: usize,
}

impl EmbedBudget {
    fn record(&mut self, text: &str) {
        self.used += text.chars().count();
    }

    fn remaining(&self) -> usize {
        MAX_TOTAL_DESC_CHARS.saturating_sub(self.used)
    }
}

/// Fold a chain of embeds into a `CreateReply`. `poise::CreateReply` in
/// 0.6 doesn't expose an `.embeds(vec)` builder — only `.embed(one)`
/// which pushes — so we feed them in via a fold.
fn embeds_reply(embeds: Vec<CreateEmbed>) -> CreateReply {
    embeds
        .into_iter()
        .fold(CreateReply::default(), |reply, e| reply.embed(e))
}

fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    let m = secs / 60;
    let s = secs % 60;
    if m == 0 {
        format!("{s}s")
    } else {
        format!("{m}m {s:02}s")
    }
}

/// Char-aware truncation with an ellipsis. Discord renders multibyte
/// glyphs (e.g. status-effect names with apostrophes via slug→display)
/// so a byte-based slice could split a codepoint.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}
