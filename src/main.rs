use anyhow::{bail, Result};
use clap::Parser;
use poise::serenity_prelude as serenity;
use sqlx::PgPool;
use tracing::{error, info};

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
}

pub type BotError = Box<dyn std::error::Error + Send + Sync>;
pub type BotContext<'a> = poise::Context<'a, BotData, BotError>;

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

    init_tracing();

    let config = config::Config::from_env()?;
    info!(config = ?config, "loaded config");

    match cli.command.unwrap_or(CliCommand::Bot) {
        CliCommand::Bot => run_bot(config).await,
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

async fn run_bot(config: config::Config) -> Result<()> {
    let pool = db::create_pool(&config.database_url).await?;
    info!("connected to database");

    r4_migration_preflight(&pool).await?;

    sqlx::migrate!("./migrations").run(&pool).await?;
    info!("migrations applied");

    templates::load_and_seed(&pool).await?;

    let token = config.discord_token.clone();
    let test_guild = config.discord_test_guild_id.map(serenity::GuildId::new);

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: commands::all(),
            command_check: Some(|ctx| Box::pin(ensure_setup(ctx))),
            on_error: |err| Box::pin(on_error(err)),
            event_handler: |ctx, event, _framework, data| {
                Box::pin(async move {
                    if let serenity::FullEvent::InteractionCreate { interaction } = event {
                        match interaction {
                            serenity::Interaction::Component(mci) => {
                                handlers::component::handle(ctx, mci, data).await?;
                            }
                            serenity::Interaction::Modal(modal) => {
                                handlers::modal::handle(ctx, modal, data).await?;
                            }
                            _ => {}
                        }
                    }
                    Ok(())
                })
            },
            ..Default::default()
        })
        .setup(move |ctx, _ready, framework| {
            Box::pin(async move {
                if let Some(guild_id) = test_guild {
                    poise::builtins::register_in_guild(
                        ctx,
                        &framework.options().commands,
                        guild_id,
                    )
                    .await?;
                    info!(%guild_id, "registered commands in test guild");
                } else {
                    poise::builtins::register_globally(ctx, &framework.options().commands).await?;
                    info!("registered commands globally");
                }
                Ok(BotData { db: pool, config })
            })
        })
        .build();

    let intents = serenity::GatewayIntents::non_privileged()
        | serenity::GatewayIntents::GUILD_MEMBERS
        | serenity::GatewayIntents::MESSAGE_CONTENT;

    let mut client = serenity::Client::builder(token, intents)
        .framework(framework)
        .await?;

    info!("starting bot");
    client.start().await?;
    Ok(())
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
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new("starship=info,serenity=warn,sqlx=warn,info")
    });

    let json = std::env::var("RUST_LOG_FORMAT").as_deref() == Ok("json");

    if json {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .with_target(true)
            .with_current_span(true)
            .with_span_list(false)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(true)
            .init();
    }
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
