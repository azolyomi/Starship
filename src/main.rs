use anyhow::Result;
use clap::Parser;
use poise::serenity_prelude as serenity;
use sqlx::PgPool;
use tracing::info;

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
    }
}

async fn run_bot(config: config::Config) -> Result<()> {
    let pool = db::create_pool(&config.database_url).await?;
    info!("connected to database");

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
