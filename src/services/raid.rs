use anyhow::Result;
use poise::serenity_prelude as serenity;
use sqlx::PgPool;

use crate::db::models::{DungeonTemplate, Headcount, Run, SlotClaim, Tier};
use crate::db::self_organize::ClaimOutcome;
use crate::embeds::headcount::emoji_rt;
use crate::services::{reactions, voice};
use crate::{db, embeds, guild_id_i64, BotContext};

/// Outcome of [`start_headcount_inner`]. The slot may already be held by
/// another HC or run in self-organize-enabled tiers, in which case the
/// caller renders a friendly "another raid is already up" message and
/// the headcount is never created.
pub enum StartHeadcountOutcome {
    Started(Headcount),
    SlotInUse(SlotClaim),
}

/// Prefix the headcount/run message with `@here` plus the dungeon's
/// notification role (if any). Discord doesn't render `@here` from a bot
/// unless `allowed_mentions` opts in via [`allow_here_and_role`].
fn ping_content(notification_role_id: Option<i64>) -> String {
    match notification_role_id {
        Some(rid) => format!("@here <@&{rid}>"),
        None => "@here".to_string(),
    }
}

/// Bot mentions are silent by default — Discord only fires pings for
/// mention types listed in `allowed_mentions`. We always allow `@here`
/// (no `@everyone` flag set, but `everyone(true)` covers both per
/// Discord's API: it controls the `everyone` parse type which gates
/// `@here` and `@everyone` together) plus the specific notification
/// role if one is configured.
fn allow_here_and_role(notification_role_id: Option<i64>) -> serenity::CreateAllowedMentions {
    let mut allowed = serenity::CreateAllowedMentions::new().everyone(true);
    if let Some(rid) = notification_role_id {
        allowed = allowed.roles(vec![serenity::RoleId::new(rid as u64)]);
    }
    allowed
}

/// Slash-command entry point. Thin wrapper over [`start_headcount_inner`]
/// that pulls `guild_id`/`leader_id` off `BotContext` and translates the
/// result into the slash command's `Result<()>` shape, with a user-facing
/// reply on `SlotInUse`.
#[tracing::instrument(
    name = "start_headcount",
    skip_all,
    fields(
        guild_id = ctx.guild_id().map(|g| g.get()),
        leader_id = ctx.author().id.get(),
        tier = %tier.name,
        dungeon = %template.name,
        channel_id,
    ),
)]
pub async fn start_headcount(
    ctx: BotContext<'_>,
    tier: &Tier,
    template: &DungeonTemplate,
    channel_id: i64,
) -> Result<()> {
    let pool = &ctx.data().db;
    let serenity_ctx = ctx.serenity_context();
    let guild_id = guild_id_i64(ctx);
    let leader_id = ctx.author().id.get() as i64;

    match start_headcount_inner(
        serenity_ctx,
        pool,
        guild_id,
        leader_id,
        tier,
        template,
        channel_id,
        false,
    )
    .await?
    {
        StartHeadcountOutcome::Started(_) => Ok(()),
        StartHeadcountOutcome::SlotInUse(holder) => {
            // Only reachable in tiers with self-organize enabled, where
            // the slot lock applies to staff /headcount too. The lock
            // could be held by either a self-organized HC/run or another
            // staff HC started concurrently in a different shard.
            ctx.say(format!(
                "Can't start a headcount: another raid for **{}** is already up (led by <@{}>).",
                template.display_name, holder.leader_user_id,
            ))
            .await?;
            Ok(())
        }
    }
}

/// Post a headcount embed to the tier's runs channel, create the DB row,
/// and attach native reactions for each required item. R4: no more
/// per-user DB tracking — the reactions on the message itself are the
/// signup UI.
///
/// In tiers with `enable_self_organization` set, this also writes a
/// row to `self_organize_slot_claims` in the same transaction as the HC
/// insert — a concurrent click that lost the race observes
/// [`StartHeadcountOutcome::SlotInUse`] without an HC ever existing.
#[allow(clippy::too_many_arguments)]
pub async fn start_headcount_inner(
    serenity_ctx: &serenity::Context,
    pool: &PgPool,
    guild_id: i64,
    leader_id: i64,
    tier: &Tier,
    template: &DungeonTemplate,
    channel_id: i64,
    is_self_organized: bool,
) -> Result<StartHeadcountOutcome> {
    // location/party are not collected at headcount-create time — the leader
    // fills them in via the modal that opens on the Start Run button. Pass
    // None so the row's columns stay NULL until then.
    let mut tx = pool.begin().await?;
    let hc = db::headcount::create_tx(
        &mut tx,
        guild_id,
        tier.id,
        template.id,
        channel_id,
        leader_id,
        None,
        None,
        is_self_organized,
    )
    .await?;

    // Slot-claim is only written when the tier opts into self-organize
    // mode. Staff-led HCs in non-self-organize tiers don't participate
    // in the slot lock at all (legacy behavior preserved).
    if tier.enable_self_organization {
        match db::self_organize::claim_for_headcount(
            &mut tx,
            guild_id,
            tier.id,
            template.id,
            hc.id,
            leader_id,
            is_self_organized,
        )
        .await?
        {
            ClaimOutcome::Acquired(_) => {}
            ClaimOutcome::Conflict(holder) => {
                tx.rollback().await?;
                tracing::info!(
                    leader_id = holder.leader_user_id,
                    holder_hc_id = ?holder.headcount_id,
                    holder_run_id = ?holder.run_id,
                    "self-organize slot already held; refusing start",
                );
                return Ok(StartHeadcountOutcome::SlotInUse(holder));
            }
        }
    }

    tx.commit().await?;
    tracing::info!(hc_id = hc.id, "headcount created");

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

    let role_id = db::dungeon::get_notification_role(pool, guild_id, &template.name).await?;
    let create = serenity::CreateMessage::new()
        .add_embed(embed)
        .components(components)
        .content(ping_content(role_id))
        .allowed_mentions(allow_here_and_role(role_id));

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

    Ok(StartHeadcountOutcome::Started(Headcount {
        message_id: msg.id.get() as i64,
        ..hc
    }))
}

/// Post a run embed. Reactions from the source headcount already carry the
/// signup state, so this does *not* attach native reactions to the run
/// message — it's a plain announcement with a Control Panel button.
///
/// The slash-command `/run` flow always lands here with
/// `is_self_organized = false`; the HC->Run convert path uses
/// [`finalize_run_post_create`] directly so it can run the run insert in
/// the same transaction as the slot-claim swap.
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
        false,
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

    finalize_run_post_create(serenity_ctx, pool, &mut run, template, raid_channel_id).await?;
    Ok(run)
}

/// Everything that happens *after* a run row exists in the DB: temp VC
/// creation, embed render, message post, and the follow-up UPDATEs that
/// stamp `voice_channel_id` and `message_id` onto the row.
///
/// Mutates `run` in place so callers always have an up-to-date struct
/// after the call returns.
///
/// Extracted from `start_run` so the HC->Run convert path can call it
/// after running `db::run::create_tx` + `claim_swap_to_run` inside its
/// own transaction. The non-tx work (Discord HTTP, voice channel CRUD)
/// always runs *outside* a tx — Postgres connections are precious and
/// holding one across a 2-second Discord call is wasteful.
pub async fn finalize_run_post_create(
    serenity_ctx: &serenity::Context,
    pool: &PgPool,
    run: &mut Run,
    template: &DungeonTemplate,
    raid_channel_id: i64,
) -> Result<()> {
    if template.requires_vc {
        let vc_name = format!("{} #{}", template.display_name, run.id);
        match voice::create_temp_vc(
            serenity_ctx,
            serenity::GuildId::new(run.guild_id as u64),
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

    let emoji_map = db::emoji::get_all_as_map(pool).await?;
    let bag_tiers = db::loot::list_bag_tiers(pool).await?;
    let threshold = db::loot::get_threshold(pool, run.guild_id).await?;

    let (embed, components) = embeds::run::build(run, template, &emoji_map, &bag_tiers, &threshold);

    let role_id = db::dungeon::get_notification_role(pool, run.guild_id, &template.name).await?;
    let create = serenity::CreateMessage::new()
        .add_embed(embed)
        .components(components)
        .content(ping_content(role_id))
        .allowed_mentions(allow_here_and_role(role_id));

    let channel = serenity::ChannelId::new(raid_channel_id as u64);
    let msg = channel.send_message(serenity_ctx, create).await?;
    db::run::set_message_id(pool, run.id, msg.id.get() as i64).await?;
    run.message_id = msg.id.get() as i64;

    Ok(())
}
