use anyhow::Result;
use poise::serenity_prelude as serenity;
use sqlx::PgPool;

use crate::db::headcount::CreateOutcome;
use crate::db::models::{DungeonTemplate, Headcount, Run, Tier};
use crate::embeds::headcount::emoji_rt;
use crate::services::{reactions, voice};
use crate::{db, embeds, guild_id_i64, BotContext};

/// Outcome of [`start_headcount_inner`]. The slot may already be held by
/// another HC, in which case the caller renders a friendly "another HC
/// is already up" message and the headcount is never created.
pub enum StartHeadcountOutcome {
    Started(Headcount),
    SlotInUse(Headcount),
}

/// Prefix the headcount/run message with `@here` and/or the dungeon's
/// notification role, gated by the per-dungeon `ping_here` toggle and
/// the presence of a bound role. Discord doesn't render `@here` from a
/// bot unless `allowed_mentions` opts in via [`allow_here_and_role`].
fn ping_content(notification_role_id: Option<i64>, ping_here: bool) -> String {
    match (ping_here, notification_role_id) {
        (true, Some(rid)) => format!("@here <@&{rid}>"),
        (true, None) => "@here".to_string(),
        (false, Some(rid)) => format!("<@&{rid}>"),
        (false, None) => String::new(),
    }
}

/// Bot mentions are silent by default — Discord only fires pings for
/// mention types listed in `allowed_mentions`. `everyone(true)` controls
/// the `everyone` parse type which gates `@here` and `@everyone`
/// together. We only enable that flag when the dungeon has `ping_here`
/// on; rolemention is opt-in per role.
fn allow_here_and_role(
    notification_role_id: Option<i64>,
    ping_here: bool,
) -> serenity::CreateAllowedMentions {
    let mut allowed = serenity::CreateAllowedMentions::new().everyone(ping_here);
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
    )
    .await?
    {
        StartHeadcountOutcome::Started(_) => Ok(()),
        StartHeadcountOutcome::SlotInUse(holder) => {
            // The slot lock (UNIQUE index on active headcounts) is
            // universal — concurrent /hc clicks for the same dungeon
            // race here, and exactly one wins.
            ctx.say(format!(
                "Can't start a headcount: another headcount for **{}** is already up (led by <@{}>).",
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
/// The slot lock is enforced by a UNIQUE index on
/// `headcounts(guild_id, tier_id, dungeon_template_id)`. A concurrent
/// click that loses the race observes [`StartHeadcountOutcome::SlotInUse`]
/// without an HC ever existing for the loser.
///
/// `location` / `party` are stashed at create time when the caller
/// already collected them (e.g. the start-run-UI modal); the slash-
/// command path passes `None` and lets the leader fill them in at
/// HC->Run convert time.
#[allow(clippy::too_many_arguments)]
pub async fn start_headcount_inner(
    serenity_ctx: &serenity::Context,
    pool: &PgPool,
    guild_id: i64,
    leader_id: i64,
    tier: &Tier,
    template: &DungeonTemplate,
    channel_id: i64,
) -> Result<StartHeadcountOutcome> {
    let mut tx = pool.begin().await?;
    let outcome = db::headcount::create_tx(
        &mut tx,
        guild_id,
        tier.id,
        template.id,
        channel_id,
        leader_id,
        None,
        None,
    )
    .await?;

    let hc = match outcome {
        CreateOutcome::Created(hc) => hc,
        CreateOutcome::SlotInUse => {
            tx.rollback().await?;
            // Re-query the holder so we can name the leader. Best effort
            // — if the holder has cleared by the time we read, fall back
            // to a generic message via a synthetic Headcount with
            // leader_user_id = 0 (the user_message handles it gracefully
            // enough; this race is vanishingly rare).
            let holder = db::headcount::slot_holder(pool, guild_id, tier.id, template.id)
                .await?
                .unwrap_or(Headcount {
                    id: 0,
                    guild_id,
                    tier_id: tier.id,
                    dungeon_template_id: template.id,
                    channel_id,
                    message_id: 0,
                    leader_user_id: 0,
                    location: None,
                    party: None,
                    created_at: chrono::Utc::now(),
                });
            tracing::info!(
                holder_leader_id = holder.leader_user_id,
                holder_hc_id = holder.id,
                "headcount slot already held; refusing start",
            );
            return Ok(StartHeadcountOutcome::SlotInUse(holder));
        }
    };

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

    let (role_id, ping_here) =
        db::dungeon::get_notification_settings(pool, guild_id, &template.name).await?;
    let create = serenity::CreateMessage::new()
        .add_embed(embed)
        .components(components)
        .content(ping_content(role_id, ping_here))
        .allowed_mentions(allow_here_and_role(role_id, ping_here));

    let channel = serenity::ChannelId::new(channel_id as u64);
    let msg = channel.send_message(serenity_ctx, create).await?;
    if let Err(e) = db::headcount::set_message_id(pool, hc.id, msg.id.get() as i64).await {
        // The HC row is committed but its message_id is still 0, and the
        // Discord message exists. The boot sweep would later delete the
        // row (correct) but the message would persist as a confusing
        // ghost: clicking its buttons no-ops, reactions go nowhere.
        // Compensate by deleting the message we just posted so DB and
        // Discord state are aligned again. If the delete also fails,
        // there's nothing more to do — log loudly and surface the
        // original error.
        if let Err(del_err) = msg.delete(&serenity_ctx.http).await {
            tracing::error!(
                set_id_error = ?e,
                delete_error = ?del_err,
                hc_id = hc.id,
                message_id = msg.id.get(),
                "set_message_id failed AND compensating delete failed; \
                 ghost HC message left in channel until manual cleanup",
            );
        } else {
            tracing::warn!(
                set_id_error = ?e,
                hc_id = hc.id,
                message_id = msg.id.get(),
                "set_message_id failed; compensating delete succeeded",
            );
        }
        return Err(e);
    }

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

/// Everything that happens *after* a run row exists in the DB: temp VC
/// creation, embed render, message post, and the follow-up UPDATEs that
/// stamp `voice_channel_id` and `message_id` onto the row.
///
/// Mutates `run` in place so callers always have an up-to-date struct
/// after the call returns.
///
/// The HC->Run convert path calls this after running
/// `db::run::create_tx` + `claim_swap_to_run` inside its own transaction.
/// The non-tx work (Discord HTTP, voice channel CRUD) always runs
/// *outside* a tx — Postgres connections are precious and holding one
/// across a 2-second Discord call is wasteful.
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

    let (role_id, ping_here) =
        db::dungeon::get_notification_settings(pool, run.guild_id, &template.name).await?;
    let create = serenity::CreateMessage::new()
        .add_embed(embed)
        .components(components)
        .content(ping_content(role_id, ping_here))
        .allowed_mentions(allow_here_and_role(role_id, ping_here));

    let channel = serenity::ChannelId::new(raid_channel_id as u64);
    let msg = channel.send_message(serenity_ctx, create).await?;
    if let Err(e) = db::run::set_message_id(pool, run.id, msg.id.get() as i64).await {
        // Same compensation as `start_headcount_inner`: the run row is
        // already committed with `message_id = 0`, so the boot sweep
        // would delete it. Without this, the just-posted message
        // becomes an orphan no-op. Delete it so DB and Discord agree.
        if let Err(del_err) = msg.delete(&serenity_ctx.http).await {
            tracing::error!(
                set_id_error = ?e,
                delete_error = ?del_err,
                run_id = run.id,
                message_id = msg.id.get(),
                "set_message_id failed AND compensating delete failed; \
                 ghost run message left in channel until manual cleanup",
            );
        } else {
            tracing::warn!(
                set_id_error = ?e,
                run_id = run.id,
                message_id = msg.id.get(),
                "set_message_id failed; compensating delete succeeded",
            );
        }
        return Err(e);
    }
    run.message_id = msg.id.get() as i64;

    Ok(())
}

/// Tear down a live run: delete the row, delete the temp VC, edit the
/// public message to its ended state, post the audit log line, and
/// refresh the start-run-UI listing.
///
/// `ended_by`:
/// - `Some(uid)` — user-driven End click. Audit log mentions the user.
/// - `None` — auto-end (idle timeout). Audit log credits the bot.
///
/// Returns `Ok(false)` when the run row was already deleted by another
/// path (concurrent click, prior auto-end, manual SQL surgery). Past
/// the DB delete, every Discord-side step is best-effort: failures log
/// and continue rather than bubbling, since the row delete is the
/// canonical "ended" state.
pub async fn end_run(
    serenity_ctx: &serenity::Context,
    pool: &PgPool,
    run: &Run,
    ended_by: Option<serenity::UserId>,
) -> Result<bool> {
    let row_existed = db::run::delete(pool, run.id).await?;
    if !row_existed {
        return Ok(false);
    }

    if let Some(vc_id) = run.voice_channel_id {
        voice::delete_temp_vc(&serenity_ctx.http, serenity::ChannelId::new(vc_id as u64)).await;
    }

    let template = match db::dungeon::get_by_id(pool, run.dungeon_template_id).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = ?e, run_id = run.id, "failed to load template for ended embed");
            None
        }
    };

    if let Some(template) = template.as_ref() {
        let emoji_map = match db::emoji::get_all_as_map(pool).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    error = ?e,
                    run_id = run.id,
                    "failed to load emoji map for ended embed",
                );
                Default::default()
            }
        };
        let ended_embed = embeds::run::build_ended(run, template, &emoji_map);
        if let Err(e) = serenity::ChannelId::new(run.channel_id as u64)
            .edit_message(
                &serenity_ctx.http,
                serenity::MessageId::new(run.message_id as u64),
                serenity::EditMessage::new()
                    .add_embed(ended_embed)
                    .components(vec![]),
            )
            .await
        {
            tracing::warn!(
                error = ?e,
                run_id = run.id,
                channel_id = run.channel_id,
                message_id = run.message_id,
                "failed to update run message to ended state",
            );
        }
    }

    if let Some(template) = template.as_ref() {
        if let Ok(Some(guild)) = db::guild::get(pool, run.guild_id).await {
            if let Some(log_id) = guild.log_channel_id {
                let actor = match ended_by {
                    Some(uid) => format!("<@{}>", uid.get()),
                    None => "**Starship** (idle timeout)".to_string(),
                };
                let content = format!(
                    "Run #{id} ({name}) ended by {actor}.",
                    id = run.id,
                    name = template.display_name,
                );
                if let Err(e) = serenity::ChannelId::new(log_id as u64)
                    .send_message(
                        &serenity_ctx.http,
                        serenity::CreateMessage::new().content(content),
                    )
                    .await
                {
                    tracing::warn!(
                        error = ?e,
                        run_id = run.id,
                        log_channel_id = log_id,
                        "failed to write audit log entry for ended run",
                    );
                }
            }
        }
    }

    if let Ok(Some(tier)) = db::tier::get_by_id(pool, run.tier_id).await {
        if tier.enable_start_run_ui {
            if let Err(e) =
                crate::services::start_run_ui_listing::refresh_listing(serenity_ctx, pool, &tier)
                    .await
            {
                tracing::warn!(
                    error = ?e,
                    tier_id = tier.id,
                    "failed to refresh start-run-UI listing after run end",
                );
            }
        }
    }

    Ok(true)
}
