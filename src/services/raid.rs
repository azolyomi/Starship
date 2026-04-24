use anyhow::Result;
use poise::serenity_prelude as serenity;
use sqlx::PgPool;

use crate::db::models::{DungeonTemplate, Run, Tier};
use crate::embeds::headcount::emoji_rt;
use crate::services::{reactions, voice};
use crate::{db, embeds, BotContext};

/// Post a headcount embed to the tier's runs channel, create the DB row,
/// and attach native reactions for each required item. R4: no more
/// per-user DB tracking — the reactions on the message itself are the
/// signup UI.
pub async fn start_headcount(
    ctx: BotContext<'_>,
    tier: &Tier,
    template: &DungeonTemplate,
    channel_id: i64,
) -> Result<()> {
    let pool = &ctx.data().db;
    let serenity_ctx = ctx.serenity_context();
    let guild_id = ctx.guild_id().unwrap().get() as i64;
    let leader_id = ctx.author().id.get() as i64;

    let hc = db::headcount::create(pool, guild_id, tier.id, template.id, channel_id, leader_id)
        .await?;

    let reactions_list = db::dungeon::get_reactions(pool, template.id).await?;
    let emoji_map = db::emoji::get_all_as_map(pool).await?;
    let bag_tiers = db::loot::list_bag_tiers(pool).await?;
    let threshold = db::loot::get_threshold(pool, guild_id).await?;

    let (embed, components) = embeds::headcount::build(
        hc.id,
        template,
        &reactions_list,
        &emoji_map,
        leader_id as u64,
        &bag_tiers,
        &threshold,
    );

    let mut create = serenity::CreateMessage::new()
        .add_embed(embed)
        .components(components);
    if let Some(role_id) = template.notification_role_id {
        create = create.content(format!("<@&{role_id}>"));
    }

    let channel = serenity::ChannelId::new(channel_id as u64);
    let msg = channel.send_message(serenity_ctx, create).await?;
    db::headcount::set_message_id(pool, hc.id, msg.id.get() as i64).await?;

    attach_signup_reactions(
        &serenity_ctx.http,
        channel,
        msg.id,
        &reactions_list,
        &emoji_map,
        leader_id as u64,
    )
    .await;

    Ok(())
}

/// Post a run embed to the tier's runs channel, create the DB row, and
/// attach native reactions for each required item.
///
/// Low-level shape (serenity ctx + pool) so it's callable from both the
/// `/run` slash command and the `hc:<id>:start` component handler.
pub async fn start_run(
    serenity_ctx: &serenity::Context,
    pool: &PgPool,
    guild_id: i64,
    tier: &Tier,
    template: &DungeonTemplate,
    raid_channel_id: i64,
    leader_user_id: i64,
    headcount_id: Option<i32>,
) -> Result<Run> {
    let mut run = db::run::create(
        pool,
        guild_id,
        tier.id,
        template.id,
        headcount_id,
        raid_channel_id,
        leader_user_id,
        template.requires_vc,
    )
    .await?;

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
                    error = %e,
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
    if let Some(role_id) = template.notification_role_id {
        create = create.content(format!("<@&{role_id}>"));
    }

    let channel = serenity::ChannelId::new(raid_channel_id as u64);
    let msg = channel.send_message(serenity_ctx, create).await?;
    db::run::set_message_id(pool, run.id, msg.id.get() as i64).await?;

    attach_signup_reactions(
        &serenity_ctx.http,
        channel,
        msg.id,
        &reactions_list,
        &emoji_map,
        leader_user_id as u64,
    )
    .await;

    Ok(Run {
        message_id: msg.id.get() as i64,
        ..run
    })
}

/// Resolve each `DungeonReaction` to a `ReactionType`, attach it via the
/// retry helper, and ping the organizer with a summary of any that failed
/// after all retries. Reactions with no resolvable emoji (logical name
/// not yet synced) are silently skipped — the next `sync-wiki` will fix
/// them on the next raid, and we don't want to block the raid posting
/// just because one icon is missing.
async fn attach_signup_reactions(
    http: &serenity::Http,
    channel_id: serenity::ChannelId,
    message_id: serenity::MessageId,
    reactions_list: &[crate::db::models::DungeonReaction],
    emoji_map: &std::collections::HashMap<String, crate::db::models::BotEmoji>,
    organizer_id: u64,
) {
    let resolved: Vec<serenity::ReactionType> = reactions_list
        .iter()
        .filter_map(|r| emoji_rt(&r.emoji, emoji_map))
        .collect();

    let failures =
        reactions::attach_reactions(http, channel_id, message_id, &resolved).await;
    reactions::ping_organizer_on_failure(http, channel_id, organizer_id, message_id, &failures)
        .await;
}
