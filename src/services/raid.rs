use std::collections::HashMap;

use anyhow::Result;
use poise::serenity_prelude as serenity;
use sqlx::PgPool;

use crate::db::models::{DungeonTemplate, Run, Tier};
use crate::{db, embeds, BotContext};

/// Post a headcount embed to the tier's headcount channel and create the DB row.
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

    // Create the DB row with a placeholder message_id.
    let hc = db::headcount::create(pool, guild_id, tier.id, template.id, channel_id, leader_id)
        .await?;

    let reactions = db::dungeon::get_reactions(pool, template.id).await?;
    let emoji_map = db::emoji::get_all_as_map(pool).await?;
    let counts: HashMap<_, _> = HashMap::new(); // empty on creation
    let bag_tiers = db::loot::list_bag_tiers(pool).await?;
    let threshold = db::loot::get_threshold(pool, guild_id, template.id).await?;

    let (embed, components) = embeds::headcount::build(
        hc.id,
        template,
        &reactions,
        &counts,
        &emoji_map,
        leader_id as u64,
        &bag_tiers,
        &threshold,
    );

    let msg = serenity::ChannelId::new(channel_id as u64)
        .send_message(
            serenity_ctx,
            serenity::CreateMessage::new()
                .add_embed(embed)
                .components(components),
        )
        .await?;

    db::headcount::set_message_id(pool, hc.id, msg.id.get() as i64).await?;

    Ok(())
}

/// Post a run embed to the tier's raid channel (or headcount channel if no
/// raid channel is configured yet) and create the DB row. If `headcount_id`
/// is supplied, migrates confirmed reactions from the headcount into
/// `run_participants` so users don't have to re-click Join.
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
    let run = db::run::create(
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

    // Seed participants from the source headcount, if any. Users who
    // declared an item with requires_confirmation carry their confirm flag;
    // plain "interested" reactions map to a NULL dungeon_reaction_id so
    // they appear in the "Joined" roster without a declared item.
    if let Some(hc_id) = headcount_id {
        migrate_headcount_reactions(pool, hc_id, run.id).await?;
    }

    let reactions = db::dungeon::get_reactions(pool, template.id).await?;
    let emoji_map = db::emoji::get_all_as_map(pool).await?;
    let participants = db::run::list_participants(pool, run.id).await?;
    let bag_tiers = db::loot::list_bag_tiers(pool).await?;
    let threshold = db::loot::get_threshold(pool, guild_id, template.id).await?;

    let (embed, components) = embeds::run::build(
        &run,
        template,
        &reactions,
        &participants,
        &emoji_map,
        &bag_tiers,
        &threshold,
    );

    let mut create = serenity::CreateMessage::new()
        .add_embed(embed)
        .components(components);

    // Ping the notification role if the template has one. Keeps the embed
    // self-contained while still nudging subscribers.
    if let Some(role_id) = template.notification_role_id {
        create = create.content(format!("<@&{role_id}>"));
    }

    let msg = serenity::ChannelId::new(raid_channel_id as u64)
        .send_message(serenity_ctx, create)
        .await?;

    db::run::set_message_id(pool, run.id, msg.id.get() as i64).await?;

    Ok(Run {
        message_id: msg.id.get() as i64,
        ..run
    })
}

async fn migrate_headcount_reactions(pool: &PgPool, hc_id: i32, run_id: i32) -> Result<()> {
    // Pull (dungeon_reaction_id, user_id, confirmed, requires_confirmation)
    // in one query so we can decide per-row whether it becomes a declared
    // item or a plain Join.
    let rows = sqlx::query!(
        r#"
        SELECT hr.user_id, hr.dungeon_reaction_id, hr.confirmed,
               dr.requires_confirmation
        FROM headcount_reactions hr
        JOIN dungeon_reactions dr ON dr.id = hr.dungeon_reaction_id
        WHERE hr.headcount_id = $1
        "#,
        hc_id
    )
    .fetch_all(pool)
    .await?;

    for row in rows {
        let (reaction_id, confirmed) = if row.requires_confirmation {
            (Some(row.dungeon_reaction_id), row.confirmed)
        } else {
            (None, false)
        };
        db::run::add_participant(pool, run_id, row.user_id, reaction_id, confirmed).await?;
    }

    Ok(())
}
