use poise::CreateReply;

use poise::serenity_prelude as serenity;

use crate::{
    db, guild_id_i64,
    services::permission::Action,
    services::{channels as channel_svc, permission as perm_svc, raid, raid_gates},
    BotContext, BotError,
};

async fn autocomplete_dungeon<'a>(
    ctx: BotContext<'_>,
    partial: &'a str,
) -> impl Iterator<Item = serenity::AutocompleteChoice> + 'a {
    let guild_id = match ctx.guild_id() {
        Some(id) => id.get() as i64,
        None => return Vec::new().into_iter(),
    };
    let needle = partial.to_lowercase();
    // Show the union of dungeons visible in at least one of this guild's
    // tiers — the tier arg isn't bound yet at autocomplete time, so we
    // can't filter to a specific tier. Post-resolution check rejects
    // dungeons that aren't visible in the tier the user actually picks.
    db::tier::list_visible_dungeons_any_tier(&ctx.data().db, guild_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(move |d| {
            d.display_name.to_lowercase().contains(&needle)
                || d.name.to_lowercase().contains(&needle)
        })
        .map(|d| serenity::AutocompleteChoice::new(d.display_name, d.name))
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

/// Build the `/hc` slash command — a shorter alias of `/headcount` that
/// lands on the same handler. Two registrations of one Command would
/// collide on `identifying_name` (which is what poise uses to pair an
/// interaction back to its command); cloning + retitling keeps each
/// registration distinct without duplicating the handler body.
pub fn hc() -> poise::Command<crate::BotData, crate::BotError> {
    let mut cmd = headcount();
    cmd.name = "hc".to_string();
    cmd.qualified_name = "hc".to_string();
    cmd.identifying_name = "hc".to_string();
    cmd
}

/// Start a headcount for a dungeon. Reachable as `/headcount` or `/hc`.
#[poise::command(slash_command, guild_only)]
pub async fn headcount(
    ctx: BotContext<'_>,
    #[description = "Dungeon to headcount for"]
    #[autocomplete = "autocomplete_dungeon"]
    dungeon: String,
    #[description = "Tier (required if multiple tiers exist)"]
    #[autocomplete = "autocomplete_tier"]
    tier: Option<String>,
) -> Result<(), BotError> {
    // Defer immediately. `raid::start_headcount` posts a message and then
    // attaches one reaction per required item, each with up to 5 retries
    // and exponential backoff — that loop routinely exceeds Discord's
    // 3-second interaction window on slower networks. After defer,
    // every subsequent `ctx.send(...)` becomes a 15-minute followup.
    ctx.defer_ephemeral().await?;

    let guild_id = guild_id_i64(ctx);
    let pool = &ctx.data().db;

    // Resolve dungeon template.
    let Some(template) = db::dungeon::get_by_name(pool, guild_id, &dungeon).await? else {
        ctx.send(ephemeral(format!(
            "Unknown dungeon `{dungeon}`. Try the autocomplete list."
        )))
        .await?;
        return Ok(());
    };

    // Resolve tier.
    let tiers = db::tier::list(pool, guild_id).await?;
    let resolved_tier = match tier {
        Some(ref name) => match tiers.iter().find(|t| t.name.eq_ignore_ascii_case(name)) {
            Some(t) => t.clone(),
            None => {
                ctx.send(ephemeral(format!("Unknown tier `{name}`.")))
                    .await?;
                return Ok(());
            }
        },
        None => {
            if tiers.len() == 1 {
                tiers.into_iter().next().expect("len() == 1 just verified")
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

    // Visibility check: globals can be soft-disabled per-tier, and
    // guild-specifics need an explicit tier_dungeons attachment. Reject
    // before the perm check so the user gets a precise diagnostic.
    if !db::tier::is_dungeon_visible(pool, resolved_tier.id, template.id, guild_id).await? {
        ctx.send(ephemeral(format!(
            "**{}** isn't enabled for tier **{}**. \
             Ask a server admin to run `/tier add-dungeon` first.",
            template.display_name, resolved_tier.name
        )))
        .await?;
        return Ok(());
    }

    // Permission check (after we know the tier + template).
    perm_svc::require(
        ctx,
        Action::StartHeadcount,
        Some(resolved_tier.id),
        Some(template.id),
    )
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

    // Pre-flight: confirm the runs channel still exists in Discord. Catching
    // this here gives the user a friendly diagnostic instead of letting
    // raid::start_headcount INSERT a row and then fail at send_message,
    // leaving an orphan row for the sweep to clean up. Bot-side outages
    // (rate-limit / 5xx) bubble normally — only a 404 takes this branch.
    if !channel_svc::channel_exists(ctx.http(), serenity::ChannelId::new(channel_id as u64)).await?
    {
        ctx.send(ephemeral(format!(
            "Tier **{}**'s runs channel <#{channel_id}> is gone — it was probably \
             deleted in Discord. Run `/tier edit` or `/setup` to point the tier at \
             a different channel.",
            resolved_tier.name
        )))
        .await?;
        return Ok(());
    }

    // Universal start gate: slot lock, per-user cap, post-cancel cooldown,
    // stale-HC sweep. Admins / `ManageRuns` bypass the cap and the
    // cooldown but never the slot lock or the stale sweep — those are
    // structural invariants. The slash command shares this gate with the
    // sticky-button path so the protections are uniform across entry
    // points.
    let is_org = perm_svc::is_organizer_from_context(ctx, Some(resolved_tier.id)).await?;
    let caller_id = ctx.author().id.get() as i64;
    if let Some(block) = raid_gates::check_can_start(
        ctx.serenity_context(),
        pool,
        &resolved_tier,
        &template,
        caller_id,
        is_org,
    )
    .await?
    {
        ctx.send(ephemeral(block.user_message())).await?;
        return Ok(());
    }

    raid::start_headcount(ctx, &resolved_tier, &template, channel_id).await?;

    ctx.send(ephemeral(format!("Headcount started in <#{channel_id}>!")))
        .await?;

    Ok(())
}
