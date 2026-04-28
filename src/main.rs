use anyhow::{bail, Result};
use clap::Parser;
use poise::serenity_prelude as serenity;
use sqlx::PgPool;
use tokio::sync::mpsc;
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod cli;
mod commands;
mod config;
mod db;
mod embeds;
mod handlers;
mod services;
mod templates;

pub struct BotData {
    pub db: PgPool,
    pub config: config::Config,
    /// Built once at startup. Verification handlers borrow it for
    /// `realmeye.com/player/<ign>` lookups; the wiki scraper has its own
    /// short-lived client because it runs as a CLI subcommand.
    pub realmeye: services::realmeye::RealmEyeClient,
}

pub type BotError = Box<dyn std::error::Error + Send + Sync>;
pub type BotContext<'a> = poise::Context<'a, BotData, BotError>;

/// Extract the current command's guild as a serenity [`GuildId`].
///
/// All command handlers calling this go through poise's `guild_only`
/// gate, so `ctx.guild_id()` is `Some` by construction. A `None` here
/// would be an upstream bug (a command attribute missing `guild_only`),
/// not a runtime condition the caller can recover from — hence
/// `.expect()` per the project rule on invariant violations.
pub fn require_guild_id(ctx: BotContext<'_>) -> serenity::GuildId {
    ctx.guild_id()
        .expect("BotContext::guild_id() in a guild_only command")
}

/// Same as [`require_guild_id`] but pre-cast to `i64` for DB use.
///
/// `as i64` is safe for every Discord snowflake we'll ever see (timestamps
/// since 2015 epoch fit well under 2^63). Centralized here so a Phase D
/// snowflake-newtype refactor only has to replace one cast.
pub fn guild_id_i64(ctx: BotContext<'_>) -> i64 {
    require_guild_id(ctx).get() as i64
}

#[derive(Parser)]
#[command(name = "starship", about = "RotMG raid bot")]
struct Cli {
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(clap::Subcommand)]
enum CliCommand {
    /// Run the Discord bot (default).
    Bot,
    /// Scrape RealmEye wiki and sync emoji + dungeon data.
    SyncWiki {
        /// Scrape + log what would be uploaded/written, but make no Discord
        /// POSTs and no DB writes. Read-only GETs (list existing emojis,
        /// fetch wiki pages) still run.
        #[arg(long)]
        dry_run: bool,
        /// DESTRUCTIVE: before scraping, delete every application emoji
        /// owned by this bot app and TRUNCATE bot_emoji. Interactive
        /// Y/N prompt. Use after renaming a batch of logical names
        /// (e.g. the 2026-04 apostrophe slug fix) so stale names don't
        /// linger on Discord. Combine with --dry-run to preview.
        #[arg(long)]
        purge: bool,
    },
    /// Upload a single PNG as an application emoji + register it in bot_emoji.
    /// Useful when the scraper doesn't have a canonical image for an item
    /// (see R4's wine_cellar_incantation).
    UploadEmoji {
        /// Logical name used in code / dungeon_reactions.emoji (e.g.
        /// `wine_cellar_incantation`).
        #[arg(long)]
        name: String,
        /// Path to a PNG file ≤256KB, ideally 128×128.
        #[arg(long)]
        file: std::path::PathBuf,
        /// Override the Discord-side name (defaults to a sanitized form of
        /// --name). Must be ≤32 chars, alphanumeric + underscores only.
        #[arg(long)]
        discord_name: Option<String>,
        /// Category label stored on bot_emoji (free-form; `ui`, `key`,
        /// `drop`, `drop_shiny` are the conventions).
        #[arg(long)]
        category: Option<String>,
        /// Bag tier classification (`white`, `cyan`, etc.). Only relevant
        /// for drop emojis.
        #[arg(long)]
        bag_tier: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // The tracing init returns the receiver half of the error-DM channel.
    // Only the `Bot` subcommand uses it; the CLI subcommands drop it on
    // exit, which closes the channel and turns the layer into a no-op
    // for any tracing events they emit.
    let dm_receiver = init_tracing();

    let config = config::Config::from_env()?;
    info!(config = ?config, "loaded config");

    match cli.command.unwrap_or(CliCommand::Bot) {
        CliCommand::Bot => run_bot(config, dm_receiver).await,
        CliCommand::SyncWiki { dry_run, purge } => cli::sync_wiki::run(dry_run, purge).await,
        CliCommand::UploadEmoji {
            name,
            file,
            discord_name,
            category,
            bag_tier,
        } => cli::upload_emoji::run(name, file, discord_name, category, bag_tier).await,
    }
}

async fn run_bot(
    config: config::Config,
    dm_receiver: mpsc::Receiver<services::error_dm::DmEvent>,
) -> Result<()> {
    let pool = db::create_pool(&config.database_url).await?;
    info!("connected to database");

    r4_migration_preflight(&pool).await?;

    sqlx::migrate!("./migrations").run(&pool).await?;
    info!("migrations applied");

    templates::load_and_seed(&pool).await?;

    let token = config.discord_token.clone();
    let test_guild = config.discord_test_guild_id.map(serenity::GuildId::new);
    // Extract before the framework builder consumes `config`.
    let error_dm_user_ids = config.error_dm_user_ids.clone();

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: commands::all(),
            command_check: Some(|ctx| Box::pin(ensure_setup(ctx))),
            on_error: |err| Box::pin(on_error(err)),
            event_handler: |ctx, event, framework, data| {
                Box::pin(async move {
                    match event {
                        serenity::FullEvent::InteractionCreate { interaction } => match interaction
                        {
                            serenity::Interaction::Component(mci) => {
                                handlers::component::handle(ctx, mci, data).await?;
                            }
                            serenity::Interaction::Modal(modal) => {
                                handlers::modal::handle(ctx, modal, data).await?;
                            }
                            _ => {}
                        },
                        // New guild joined. In dev (test_guild set) we
                        // per-guild-register so commands appear instantly;
                        // in prod we rely on global registration, and a
                        // duplicate per-guild registration here would
                        // double every command in the picker. New prod
                        // joins eat the ~1h global propagation window.
                        serenity::FullEvent::GuildCreate {
                            guild,
                            is_new: Some(true),
                        } if data.config.discord_test_guild_id.is_some() => {
                            let commands = &framework.options().commands;
                            match poise::builtins::register_in_guild(ctx, commands, guild.id).await
                            {
                                Ok(()) => info!(
                                    guild_id = %guild.id,
                                    name = %guild.name,
                                    "registered commands in newly-joined guild",
                                ),
                                Err(e) => tracing::warn!(
                                    error = ?e,
                                    guild_id = %guild.id,
                                    "failed to register commands in newly-joined guild",
                                ),
                            }
                        }
                        _ => {}
                    }
                    Ok(())
                })
            },
            ..Default::default()
        })
        .setup(move |ctx, ready, framework| {
            Box::pin(async move {
                let commands = &framework.options().commands;
                match test_guild {
                    Some(gid) => {
                        // Dev: per-guild only, in the test guild. Skips
                        // the ~1h global propagation window and avoids
                        // polluting the global command set with WIP
                        // iterations.
                        match poise::builtins::register_in_guild(ctx, commands, gid).await {
                            Ok(()) => info!(guild_id = %gid, "registered commands in test guild"),
                            Err(e) => error!(
                                error = ?e,
                                guild_id = %gid,
                                "failed to register commands in test guild",
                            ),
                        }
                    }
                    None => {
                        // Prod: global registration. Discord caches by
                        // application id so subsequent boots are a no-op
                        // for already-known commands.
                        poise::builtins::register_globally(ctx, commands).await?;
                        info!("registered commands globally");
                        // One-time cleanup: clear per-guild registrations
                        // left over from any prior dev boot under the
                        // same application id. Without this, those
                        // per-guild commands appear alongside the new
                        // globals as duplicates in the picker. Cheap
                        // (one HTTP call per guild the bot is in) and
                        // idempotent — once cleared, future boots see
                        // an already-empty per-guild set.
                        clear_per_guild_commands(ctx, ready).await;
                    }
                }
                // Reconcile DB lifecycle rows against Discord state. Failure
                // is logged but doesn't block startup — running the bot with
                // a few orphan rows beats refusing to boot.
                if let Err(e) = services::orphan_sweep::run(ctx, &pool).await {
                    error!(error = ?e, "orphan sweep failed; continuing startup");
                }
                // Periodic idle sweeper: auto-ends runs the leader forgot
                // about after RUN_IDLE_HOURS, and auto-cancels headcounts
                // older than HC_IDLE_HOURS (which would otherwise hold a
                // self-organize slot lock indefinitely). Spawned once per
                // process, runs forever until the runtime is dropped.
                services::orphan_sweep::spawn_idle_sweeper(ctx.clone(), pool.clone());
                let realmeye =
                    services::realmeye::RealmEyeClient::new(&config.realmeye_user_agent)?;
                Ok(BotData {
                    db: pool,
                    config,
                    realmeye,
                })
            })
        })
        .build();

    let intents = serenity::GatewayIntents::non_privileged()
        | serenity::GatewayIntents::GUILD_MEMBERS
        | serenity::GatewayIntents::MESSAGE_CONTENT;

    let mut client = serenity::Client::builder(token, intents)
        .framework(framework)
        .await?;

    // Spawn the error-DM dispatch loop now that we have a Discord HTTP
    // client. Done outside the framework `setup` callback because
    // `mpsc::Receiver` isn't `Sync` and `setup`'s closure must be —
    // moving it through the closure would require a `Mutex` dance for
    // no real benefit. Doing it here also means the dispatch loop is
    // alive for any errors that fire during the framework's setup
    // (orphan sweep, command registration, etc.).
    let recipients: Vec<serenity::UserId> = error_dm_user_ids
        .into_iter()
        .map(serenity::UserId::new)
        .collect();
    services::error_dm::spawn_dispatch_loop(client.http.clone(), recipients, dm_receiver);

    info!("starting bot");
    client.start().await?;
    Ok(())
}

/// Clear per-guild slash command registrations for every guild the
/// bot is currently a member of. Used on prod startup to scrub stale
/// per-guild registrations left behind by an earlier dev boot of the
/// same Discord application — without this they coexist with the new
/// global registration and Discord shows every command twice.
///
/// Idempotent (clearing an already-empty set is a no-op on Discord's
/// side) and cheap: one HTTP call per guild. Failures are logged
/// individually so one rate-limited / kicked guild doesn't abort the
/// rest.
async fn clear_per_guild_commands(ctx: &serenity::Context, ready: &serenity::Ready) {
    // Annotated empty slice — `register_in_guild` is generic over the bot's
    // command type and rustc can't infer it from `&[]` alone.
    let empty: &[poise::Command<BotData, BotError>] = &[];
    for guild in &ready.guilds {
        match poise::builtins::register_in_guild(ctx, empty, guild.id).await {
            Ok(()) => info!(guild_id = %guild.id, "cleared per-guild commands"),
            Err(e) => tracing::warn!(
                error = ?e,
                guild_id = %guild.id,
                "failed to clear per-guild commands",
            ),
        }
    }
}

/// Refuse to apply the R4 migration (which drops `headcount_reactions` +
/// `run_participants`) if any headcounts or runs are still active. Applying
/// it mid-raid would silently discard per-user signup state without any
/// in-flight communication. Operators who have coordinated a migration
/// window can set `STARSHIP_ALLOW_MIGRATION=1` to override.
///
/// Only fires on the pre-R4 schema — detected by the lingering
/// `headcount_reactions` table. After R4 is applied, this is a no-op.
async fn r4_migration_preflight(pool: &PgPool) -> Result<()> {
    let pre_r4_name: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('public.headcount_reactions')::TEXT")
            .fetch_one(pool)
            .await?;

    if pre_r4_name.is_none() {
        return Ok(());
    }

    let active_hc: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM headcounts WHERE status = 'active'")
            .fetch_one(pool)
            .await?;
    let active_runs: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM runs WHERE status = 'active'")
            .fetch_one(pool)
            .await?;

    if active_hc + active_runs == 0 {
        return Ok(());
    }

    if std::env::var("STARSHIP_ALLOW_MIGRATION").as_deref() == Ok("1") {
        info!(
            active_hc,
            active_runs,
            "STARSHIP_ALLOW_MIGRATION=1 — applying R4 migration despite active lifecycle rows"
        );
        return Ok(());
    }

    error!(
        active_hc,
        active_runs,
        "refusing to apply R4 migration with live headcounts/runs — \
         end or cancel them in-app, or set STARSHIP_ALLOW_MIGRATION=1 to override"
    );
    bail!("R4 migration preflight: {active_hc} active headcount(s), {active_runs} active run(s) — aborting startup");
}

async fn on_error(error: poise::FrameworkError<'_, BotData, BotError>) {
    use poise::FrameworkError::*;
    match &error {
        Setup { error, .. } => {
            tracing::error!(error = ?error, "framework setup failed");
        }
        Command { error, ctx, .. } => {
            tracing::error!(
                error = ?error,
                command = %ctx.command().qualified_name,
                user_id = ctx.author().id.get(),
                guild_id = ctx.guild_id().map(|g| g.get()),
                "command failed",
            );
        }
        CommandCheckFailed { error, ctx, .. } => {
            tracing::warn!(
                error = ?error,
                command = %ctx.command().qualified_name,
                user_id = ctx.author().id.get(),
                guild_id = ctx.guild_id().map(|g| g.get()),
                "command check failed",
            );
        }
        EventHandler { error, event, .. } => {
            tracing::error!(
                error = ?error,
                event = event.snake_case_name(),
                "event handler failed",
            );
        }
        other => {
            tracing::warn!(kind = %framework_error_kind(other), "framework error");
        }
    }
    if let Err(e) = poise::builtins::on_error(error).await {
        tracing::error!(error = ?e, "error while handling framework error");
    }
}

/// Friendly variant name for `FrameworkError` cases the structured logger
/// doesn't break out individually. Avoids requiring `Debug` on `BotData`.
fn framework_error_kind(err: &poise::FrameworkError<'_, BotData, BotError>) -> &'static str {
    use poise::FrameworkError::*;
    match err {
        Setup { .. } => "setup",
        Command { .. } => "command",
        CommandCheckFailed { .. } => "command_check_failed",
        ArgumentParse { .. } => "argument_parse",
        CommandStructureMismatch { .. } => "command_structure_mismatch",
        CooldownHit { .. } => "cooldown_hit",
        MissingBotPermissions { .. } => "missing_bot_permissions",
        MissingUserPermissions { .. } => "missing_user_permissions",
        NotAnOwner { .. } => "not_an_owner",
        GuildOnly { .. } => "guild_only",
        DmOnly { .. } => "dm_only",
        NsfwOnly { .. } => "nsfw_only",
        EventHandler { .. } => "event_handler",
        DynamicPrefix { .. } => "dynamic_prefix",
        UnknownCommand { .. } => "unknown_command",
        UnknownInteraction { .. } => "unknown_interaction",
        SubcommandRequired { .. } => "subcommand_required",
        _ => "other",
    }
}

/// Configure the global tracing subscriber.
///
/// Output goes to stdout so the same binary produces useful logs under
/// `docker logs` (Compose deploy) and `journalctl -u starship` (bare-metal
/// systemd). Set `RUST_LOG_FORMAT=json` to switch to a JSON-per-line
/// format suitable for log shippers; default is the human-readable
/// pretty format.
///
/// Default filter keeps `starship=info` while quieting third-party
/// noise (`serenity=warn`, `sqlx=warn`). Override at runtime with
/// `RUST_LOG=...` per the `EnvFilter` syntax.
fn init_tracing() -> mpsc::Receiver<services::error_dm::DmEvent> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new("starship=info,serenity=warn,sqlx=warn,info")
    });

    let json = std::env::var("RUST_LOG_FORMAT").as_deref() == Ok("json");
    let (dm_layer, dm_receiver) = services::error_dm::install();

    // Compose layers via `registry()` so the fmt subscriber and the
    // error-DM layer share one filter and one event stream. Using
    // `tracing_subscriber::fmt()` directly (the previous shape) only
    // installs the fmt subscriber and gives no place to attach extra
    // layers.
    if json {
        tracing_subscriber::registry()
            .with(filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_target(true)
                    .with_current_span(true)
                    .with_span_list(false),
            )
            .with(dm_layer)
            .init();
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer().with_target(true))
            .with(dm_layer)
            .init();
    }

    dm_receiver
}

/// Framework command_check: bail out of any non-`/setup` command when the
/// guild row doesn't exist yet, with a friendly prompt to run `/setup`.
async fn ensure_setup(ctx: BotContext<'_>) -> Result<bool, BotError> {
    // `setup` is the whole point of this gate; `upload-emoji` is operator-only
    // and touches the bot-wide application-emoji set, not guild config, so it
    // works even in guilds that haven't been /setup'd yet.
    if matches!(ctx.command().name.as_str(), "setup" | "upload-emoji") {
        return Ok(true);
    }
    let Some(guild_id) = ctx.guild_id() else {
        return Ok(true); // `guild_only` commands will reject DMs on their own.
    };
    let exists = db::guild::get(&ctx.data().db, guild_id.get() as i64)
        .await?
        .is_some();
    if !exists {
        ctx.send(
            poise::CreateReply::default()
                .content("This server hasn't been set up yet. Run `/setup` first.")
                .ephemeral(true),
        )
        .await?;
        return Ok(false);
    }
    Ok(true)
}
