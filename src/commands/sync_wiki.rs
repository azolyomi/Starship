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
        .send(
            CreateReply::default()
                .content(state.render(started_at))
                .ephemeral(true),
        )
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
                if let Err(e) = handle
                    .edit(ctx, CreateReply::default().content(state.render(started_at)))
                    .await
                {
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

    let final_content = match &summary_result {
        Ok(summary) => state.render_done(started_at, summary),
        Err(e) => state.render_failed(started_at, e),
    };

    if let Err(e) = handle
        .edit(ctx, CreateReply::default().content(final_content))
        .await
    {
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
// Progress aggregator + ephemeral renderer.
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct ProgressState {
    dry_run: bool,
    purge: bool,
    phase: String,
    current_dungeon: Option<(String, usize, usize)>,
    uploads_new: usize,
    uploads_reused: usize,
    uploads_skipped: usize,
    warnings: usize,
    last_warning: Option<String>,
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
            ProgressEvent::Warning(msg) => {
                self.warnings += 1;
                self.last_warning = Some(msg);
            }
            ProgressEvent::UploadNew => self.uploads_new += 1,
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

    fn render(&self, started_at: Instant) -> String {
        let elapsed = format_elapsed(started_at.elapsed());
        let mut out = format!(
            "**Running `/sync-wiki`** [{flags}] — {elapsed} elapsed\nPhase: {phase}\n",
            flags = self.flags_line(),
            phase = self.phase,
        );
        if let Some((name, idx, total)) = &self.current_dungeon {
            out.push_str(&format!("Current: {name} ({idx}/{total})\n"));
        }
        out.push_str(&format!(
            "Uploads: {} new, {} reused",
            self.uploads_new, self.uploads_reused,
        ));
        if self.dry_run {
            out.push_str(&format!(", {} would-upload", self.uploads_skipped));
        }
        out.push('\n');
        out.push_str(&format!("Warnings: {}", self.warnings));
        if let Some(last) = &self.last_warning {
            out.push_str(&format!(" (latest: `{}`)", truncate(last, 160)));
        }
        out.push('\n');
        out
    }

    fn render_done(&self, started_at: Instant, summary: &SyncSummary) -> String {
        let elapsed = format_elapsed(started_at.elapsed());
        let mut out = format!(
            "**Done — `/sync-wiki`** [{flags}] in {elapsed}\n\
             Dungeons scraped: {dungeons}\n\
             Uploads: {new} new, {reused} reused",
            flags = self.flags_line(),
            dungeons = summary.dungeons_scraped,
            new = summary.uploads_new,
            reused = summary.uploads_reused,
        );
        if self.dry_run {
            out.push_str(&format!(
                ", {} would-upload, {} bot_emoji rows skipped",
                summary.uploads_skipped_dry, summary.upserts_skipped_dry,
            ));
        }
        out.push('\n');
        out.push_str(&format!("Warnings: {}", summary.warnings));
        if let Some(last) = &self.last_warning {
            out.push_str(&format!(" (latest: `{}`)", truncate(last, 160)));
        }
        out.push_str(
            "\n\nFull logs are in the bot's journal. \
             `templates::load_and_seed` re-reads `wiki_dump.json` on the next bot restart.",
        );
        out
    }

    fn render_failed(&self, started_at: Instant, err: &anyhow::Error) -> String {
        let elapsed = format_elapsed(started_at.elapsed());
        format!(
            "**Failed — `/sync-wiki`** [{flags}] after {elapsed}\nPhase at failure: {phase}\n\
             Error: `{err}`\nWarnings before failure: {warnings}",
            flags = self.flags_line(),
            phase = self.phase,
            err = truncate(&format!("{err:#}"), 1500),
            warnings = self.warnings,
        )
    }
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
/// so a byte-based slice can split a codepoint.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}
