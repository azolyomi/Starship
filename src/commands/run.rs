use poise::CreateReply;

use crate::{
    db,
    services::permission::{self as perm_svc, Action},
    services::raid,
    BotContext, BotError,
};

async fn autocomplete_dungeon<'a>(
    ctx: BotContext<'_>,
    partial: &'a str,
) -> impl Iterator<Item = String> + 'a {
    let guild_id = match ctx.guild_id() {
        Some(id) => id.get() as i64,
        None => return Vec::new().into_iter(),
    };
    db::dungeon::list_for_guild(&ctx.data().db, guild_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(move |d| d.name.to_lowercase().contains(&partial.to_lowercase()))
        .map(|d| d.name)
        .collect::<Vec<_>>()
        .into_iter()
}

async fn autocomplete_tier<'a>(
    ctx: BotContext<'_>,
    partial: &'a str,
) -> impl Iterator<Item = String> + 'a {
    let guild_id = match ctx.guild_id() {
        Some(id) => id.get() as i64,
        None => return Vec::new().into_iter(),
    };
    db::tier::list(&ctx.data().db, guild_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(move |t| t.name.to_lowercase().contains(&partial.to_lowercase()))
        .map(|t| t.name)
        .collect::<Vec<_>>()
        .into_iter()
}

fn ephemeral(msg: impl Into<String>) -> CreateReply {
    CreateReply::default().content(msg).ephemeral(true)
}

/// Start a run directly, skipping headcount.
#[poise::command(slash_command, guild_only)]
pub async fn run(
    ctx: BotContext<'_>,
    #[description = "Dungeon to run"]
    #[autocomplete = "autocomplete_dungeon"]
    dungeon: String,
    #[description = "Tier (required if multiple tiers exist)"]
    #[autocomplete = "autocomplete_tier"]
    tier: Option<String>,
) -> Result<(), BotError> {
    let guild_id = ctx.guild_id().unwrap().get() as i64;
    let pool = &ctx.data().db;

    let Some(template) = db::dungeon::get_by_name(pool, guild_id, &dungeon).await? else {
        ctx.send(ephemeral(format!(
            "Unknown dungeon `{dungeon}`. Try the autocomplete list."
        )))
        .await?;
        return Ok(());
    };

    let tiers = db::tier::list(pool, guild_id).await?;
    let resolved_tier = match tier {
        Some(ref name) => match tiers.iter().find(|t| t.name.eq_ignore_ascii_case(name)) {
            Some(t) => t.clone(),
            None => {
                ctx.send(ephemeral(format!("Unknown tier `{name}`."))).await?;
                return Ok(());
            }
        },
        None => {
            if tiers.len() == 1 {
                tiers.into_iter().next().unwrap()
            } else if tiers.is_empty() {
                ctx.send(ephemeral("No tiers configured — run `/setup` first."))
                    .await?;
                return Ok(());
            } else {
                let names: Vec<_> = tiers.iter().map(|t| t.name.as_str()).collect();
                ctx.send(ephemeral(format!(
                    "Multiple tiers exist: {}. Specify one with the `tier` argument.",
                    names.join(", ")
                )))
                .await?;
                return Ok(());
            }
        }
    };

    perm_svc::require(
        ctx,
        Action::StartRun,
        Some(resolved_tier.id),
        Some(template.id),
    )
    .await?;

    let Some(raid_channel_id) = resolved_tier.runs_channel_id else {
        ctx.send(ephemeral(format!(
            "Tier **{}** has no runs channel configured. Use `/setup` or \
             `/tier edit` to set one.",
            resolved_tier.name
        )))
        .await?;
        return Ok(());
    };

    raid::start_run(
        ctx.serenity_context(),
        pool,
        guild_id,
        &resolved_tier,
        &template,
        raid_channel_id,
        ctx.author().id.get() as i64,
        None,
    )
    .await?;

    ctx.send(ephemeral(format!(
        "Run started in <#{raid_channel_id}>!"
    )))
    .await?;

    Ok(())
}
