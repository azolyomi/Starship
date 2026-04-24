use poise::CreateReply;

use crate::{
    db,
    services::{permission as perm_svc, raid},
    services::permission::Action,
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

/// Start a headcount for a dungeon.
#[poise::command(slash_command, guild_only)]
pub async fn headcount(
    ctx: BotContext<'_>,
    #[description = "Dungeon to headcount for"]
    #[autocomplete = "autocomplete_dungeon"]
    dungeon: String,
    #[description = "Tier (required if multiple tiers exist)"]
    #[autocomplete = "autocomplete_tier"]
    tier: Option<String>,
    #[description = "Prefill location (carries over when the run starts)"]
    location: Option<String>,
    #[description = "Prefill party composition (carries over when the run starts)"]
    party: Option<String>,
) -> Result<(), BotError> {
    let guild_id = ctx.guild_id().unwrap().get() as i64;
    let pool = &ctx.data().db;

    // Resolve dungeon template.
    let Some(template) = db::dungeon::get_by_name(pool, guild_id, &dungeon).await? else {
        ctx.send(ephemeral(format!("Unknown dungeon `{dungeon}`. Try the autocomplete list.")))
            .await?;
        return Ok(());
    };

    // Resolve tier.
    let tiers = db::tier::list(pool, guild_id).await?;
    let resolved_tier = match tier {
        Some(ref name) => {
            match tiers.iter().find(|t| t.name.eq_ignore_ascii_case(name)) {
                Some(t) => t.clone(),
                None => {
                    ctx.send(ephemeral(format!("Unknown tier `{name}`."))).await?;
                    return Ok(());
                }
            }
        }
        None => {
            if tiers.len() == 1 {
                tiers.into_iter().next().unwrap()
            } else if tiers.is_empty() {
                ctx.send(ephemeral("No tiers configured — run `/setup` first.")).await?;
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

    // Permission check (after we know the tier + template).
    perm_svc::require(ctx, Action::StartHeadcount, Some(resolved_tier.id), Some(template.id))
        .await?;

    // Check that a runs channel is configured for this tier.
    let Some(channel_id) = resolved_tier.runs_channel_id else {
        ctx.send(ephemeral(format!(
            "Tier **{}** has no runs channel. Use `/setup` or `/tier edit` to set one.",
            resolved_tier.name
        )))
        .await?;
        return Ok(());
    };

    let location = location.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let party = party.as_deref().map(str::trim).filter(|s| !s.is_empty());

    raid::start_headcount(ctx, &resolved_tier, &template, channel_id, location, party).await?;

    ctx.send(ephemeral(format!("Headcount started in <#{channel_id}>!")))
        .await?;

    Ok(())
}
