use anyhow::{bail, Result};
use clap::Parser;
use poise::serenity_prelude as serenity;
use sqlx::PgPool;
use tracing::{error, info};

mod cli;
mod commands;
mod config;
mod curation;
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
    /// Interactively curate which reactions + drops to keep per dungeon.
    /// Writes data/curation.json, then deletes de-selected Discord emojis
    /// and DB rows. Requires a snapshot from a prior `sync-wiki` run.
    Curate {
        /// Re-prompt every dungeon (including ones already in curation.json),
        /// pre-checked with the current selections.
        #[arg(long)]
        recurate: bool,
        /// Walk the prompts and print the resulting curation.json, but don't
        /// write to disk and don't delete anything.
        #[arg(long)]
        dry_run: bool,
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

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "starship=info,warn".into()),
        )
        .init();

    let config = config::Config::from_env()?;
    info!(config = ?config, "loaded config");

    match cli.command.unwrap_or(CliCommand::Bot) {
        CliCommand::Bot => run_bot(config).await,
        CliCommand::SyncWiki { dry_run, purge } => cli::sync_wiki::run(dry_run, purge).await,
        CliCommand::Curate { recurate, dry_run } => cli::curate::run(recurate, dry_run).await,
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

    let curation = curation::Curation::load()?;
    db::dungeon::seed_builtins(&pool, &curation).await?;
    info!("built-in dungeon templates seeded");

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
                    poise::builtins::register_globally(ctx, &framework.options().commands)
                        .await?;
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
    let pre_r4_name: Option<String> = sqlx::query_scalar(
        "SELECT to_regclass('public.headcount_reactions')::TEXT",
    )
    .fetch_one(pool)
    .await?;

    if pre_r4_name.is_none() {
        return Ok(());
    }

    let active_hc: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT FROM headcounts WHERE status = 'active'",
    )
    .fetch_one(pool)
    .await?;
    let active_runs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT FROM runs WHERE status = 'active'",
    )
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
    if let Err(e) = poise::builtins::on_error(error).await {
        tracing::error!("error while handling error: {e}");
    }
}

/// Framework command_check: bail out of any non-`/setup` command when the
/// guild row doesn't exist yet, with a friendly prompt to run `/setup`.
async fn ensure_setup(ctx: BotContext<'_>) -> Result<bool, BotError> {
    if ctx.command().name == "setup" {
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
