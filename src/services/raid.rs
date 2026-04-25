use anyhow::Result;
use poise::serenity_prelude as serenity;
use sqlx::PgPool;

use crate::db::models::{DungeonTemplate, Run, Tier};
use crate::embeds::headcount::emoji_rt;
use crate::services::{reactions, voice};
use crate::{db, embeds, guild_id_i64, BotContext};

/// Post a headcount embed to the tier's runs channel, create the DB row,
/// and attach native reactions for each required item. R4: no more
/// per-user DB tracking — the reactions on the message itself are the
/// signup UI.
#[tracing::instrument(
    name = "start_headcount",
    skip_all,
    fields(
        guild_id = ctx.guild_id().map(|g| g.get()),
        leader_id = ctx.author().id.get(),
        tier = %tier.name,
        dungeon = %template.name,
        channel_id,
        hc_id = tracing::field::Empty,
    ),
)]
pub async fn start_headcount(
    ctx: BotContext<'_>,
    tier: &Tier,
    template: &DungeonTemplate,
    channel_id: i64,
    location: Option<&str>,
    party: Option<&str>,
) -> Result<()> {
    let pool = &ctx.data().db;
    let serenity_ctx = ctx.serenity_context();
    let guild_id = guild_id_i64(ctx);
    let leader_id = ctx.author().id.get() as i64;

    let hc = db::headcount::create(
        pool,
        guild_id,
        tier.id,
        template.id,
        channel_id,
        leader_id,
        location,
        party,
    )
    .await?;
    tracing::Span::current().record("hc_id", hc.id);
    tracing::info!("headcount created");

    let reactions_list = db::dungeon::get_reactions(pool, template.id).await?;
    let emoji_map = db::emoji::get_all_as_map(pool).await?;
    let bag_tiers = db::loot::list_bag_tiers(pool).await?;

    let (embed, components) = embeds::headcount::build(
        hc.id,
        template,
        &reactions_list,
        &emoji_map,
        leader_id as u64,
        &bag_tiers,
    );

    let mut create = serenity::CreateMessage::new()
        .add_embed(embed)
        .components(components);
    if let Some(role_id) =
        db::dungeon::get_notification_role(pool, guild_id, &template.name).await?
    {
        create = create.content(format!("<@&{role_id}>"));
    }

    let channel = serenity::ChannelId::new(channel_id as u64);
    let msg = channel.send_message(serenity_ctx, create).await?;
    db::headcount::set_message_id(pool, hc.id, msg.id.get() as i64).await?;

    let resolved: Vec<serenity::ReactionType> = reactions_list
        .iter()
        .filter_map(|r| emoji_rt(&r.emoji, &emoji_map))
        .collect();
    let failures =
        reactions::attach_reactions(&serenity_ctx.http, channel, msg.id, &resolved).await;
    reactions::ping_organizer_on_failure(
        &serenity_ctx.http,
        channel,
        leader_id as u64,
        msg.id,
        &failures,
    )
    .await;

    Ok(())
}

/// Post a run embed. Reactions from the source headcount already carry the
/// signup state, so this does *not* attach native reactions to the run
/// message — it's a plain announcement with a Control Panel button.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    name = "start_run",
    skip_all,
    fields(
        guild_id,
        leader_id = leader_user_id,
        tier = %tier.name,
        dungeon = %template.name,
        requires_vc = template.requires_vc,
        run_id = tracing::field::Empty,
    ),
)]
pub async fn start_run(
    serenity_ctx: &serenity::Context,
    pool: &PgPool,
    guild_id: i64,
    tier: &Tier,
    template: &DungeonTemplate,
    raid_channel_id: i64,
    leader_user_id: i64,
    location: Option<&str>,
    party: Option<&str>,
) -> Result<Run> {
    let mut run = db::run::create(
        pool,
        guild_id,
        tier.id,
        template.id,
        raid_channel_id,
        leader_user_id,
        template.requires_vc,
    )
    .await?;
    tracing::Span::current().record("run_id", run.id);
    tracing::info!("run created");

    // Prefill runs as a follow-up UPDATE so `create`'s signature stays
    // tight. Skip the round-trip entirely when nothing was prefilled.
    if location.is_some() || party.is_some() {
        if let Some(loc) = location {
            db::run::set_location(pool, run.id, Some(loc)).await?;
            run.location = Some(loc.to_string());
        }
        if let Some(p) = party {
            db::run::set_party(pool, run.id, Some(p)).await?;
            run.party = Some(p.to_string());
        }
    }

    // Temp VC for VC-required dungeons. Best-effort: if creation fails we
    // log and keep going — a raid without a VC still works, a raid that
    // failed to post doesn't.
    if template.requires_vc {
        let vc_name = format!("{} #{}", template.display_name, run.id);
        match voice::create_temp_vc(
            serenity_ctx,
            serenity::GuildId::new(guild_id as u64),
            serenity::ChannelId::new(raid_channel_id as u64),
            &vc_name,
        )
        .await
        {
            Ok(vc_id) => {
                let vc_i64 = vc_id.get() as i64;
                db::run::set_voice_channel(pool, run.id, Some(vc_i64)).await?;
                run.voice_channel_id = Some(vc_i64);
            }
            Err(e) => {
                tracing::warn!(
                    run_id = run.id,
                    error = ?e,
                    "failed to create temp VC for run; continuing without one",
                );
            }
        }
    }

    let reactions_list = db::dungeon::get_reactions(pool, template.id).await?;
    let emoji_map = db::emoji::get_all_as_map(pool).await?;
    let bag_tiers = db::loot::list_bag_tiers(pool).await?;
    let threshold = db::loot::get_threshold(pool, guild_id).await?;

    let (embed, components) = embeds::run::build(
        &run,
        template,
        &reactions_list,
        &emoji_map,
        &bag_tiers,
        &threshold,
    );

    let mut create = serenity::CreateMessage::new()
        .add_embed(embed)
        .components(components);
    if let Some(role_id) =
        db::dungeon::get_notification_role(pool, guild_id, &template.name).await?
    {
        create = create.content(format!("<@&{role_id}>"));
    }

    let channel = serenity::ChannelId::new(raid_channel_id as u64);
    let msg = channel.send_message(serenity_ctx, create).await?;
    db::run::set_message_id(pool, run.id, msg.id.get() as i64).await?;

    Ok(Run {
        message_id: msg.id.get() as i64,
        ..run
    })
}
