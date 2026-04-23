use std::collections::HashMap;

use anyhow::Result;
use poise::serenity_prelude as serenity;

use crate::db::models::{DungeonTemplate, Tier};
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

    let (embed, components) = embeds::headcount::build(
        hc.id,
        template,
        &reactions,
        &counts,
        &emoji_map,
        leader_id as u64,
        &tier.name,
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
