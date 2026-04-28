//! Component + modal routing for `run:*` custom_ids.
//!
//! R4 removed the DB-tracked Join / Leave / Confirm buttons — users declare
//! attendance via native Discord reactions on the message. The only public
//! button on a run is Control Panel, which is gated (leader or ManageRuns).
//!
//! custom_id grammar (stateless, survives restarts):
//!   run:<id>:cp                   -- open Control Panel (organizer gated)
//!   run:<id>:loc                  -- open location modal
//!   run:<id>:party                -- open party modal
//!   run:<id>:transfer             -- open UserSelect for new leader
//!   run:<id>:xfer                 -- submission of the UserSelect
//!   run:<id>:end                  -- mark ended, grey the embed

use poise::serenity_prelude as serenity;
use serenity::{
    ActionRowComponent, CreateActionRow, CreateInputText, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateModal, CreateSelectMenu, CreateSelectMenuKind,
    EditMessage, InputTextStyle, MessageId,
};

use crate::db::models::Run;
use crate::{db, embeds, services, BotData, BotError};

// ---------------------------------------------------------------------------
// Top-level dispatchers
// ---------------------------------------------------------------------------

pub async fn handle_component(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
) -> Result<(), BotError> {
    let id = &mci.data.custom_id;
    let parts: Vec<&str> = id.split(':').collect();
    if parts.len() < 3 {
        return Ok(());
    }

    let run_id: i32 = match parts[1].parse() {
        Ok(n) => n,
        Err(_) => return Ok(()),
    };

    match parts[2] {
        "cp" => handle_cp(ctx, mci, data, run_id).await,
        "loc" => handle_loc_open(ctx, mci, data, run_id).await,
        "party" => handle_party_open(ctx, mci, data, run_id).await,
        "transfer" => handle_transfer_open(ctx, mci, data, run_id).await,
        "xfer" => handle_transfer_submit(ctx, mci, data, run_id).await,
        "end" => handle_end(ctx, mci, data, run_id).await,
        _ => Ok(()),
    }
}

pub async fn handle_modal(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    data: &BotData,
) -> Result<(), BotError> {
    let id = &modal.data.custom_id;
    let parts: Vec<&str> = id.split(':').collect();
    if parts.len() < 3 {
        return Ok(());
    }

    let run_id: i32 = match parts[1].parse() {
        Ok(n) => n,
        Err(_) => return Ok(()),
    };

    match parts[2] {
        "loc" => handle_loc_submit(ctx, modal, data, run_id).await,
        "party" => handle_party_submit(ctx, modal, data, run_id).await,
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn ephemeral_msg(text: impl Into<String>) -> CreateInteractionResponse {
    CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new()
            .content(text)
            .ephemeral(true),
    )
}

async fn load_active(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    run_id: i32,
) -> Result<Option<Run>, BotError> {
    match db::run::get(&data.db, run_id).await? {
        None => {
            mci.create_response(ctx, ephemeral_msg("This run has ended."))
                .await?;
            Ok(None)
        }
        Some(run) => Ok(Some(run)),
    }
}

/// Rebuild the public run message from the current DB state. Used after the
/// location, party, or leader changes.
///
/// If the message has been deleted out-of-band (admin cleanup, channel
/// purge), the run is unmanageable from Discord's side — its buttons no
/// longer exist. Fast-path through `end_run` to release the slot claim,
/// drop the temp VC, and write the audit log immediately, instead of
/// leaving a zombie row + VC for the next 24h idle sweep / boot sweep.
async fn rebuild_and_edit_message(
    ctx: &serenity::Context,
    data: &BotData,
    run: &Run,
) -> Result<(), BotError> {
    let pool = &data.db;
    let template = db::dungeon::get_by_id(pool, run.dungeon_template_id)
        .await?
        .ok_or_else(|| format!("template {} not found", run.dungeon_template_id))?;
    let emoji_map = db::emoji::get_all_as_map(pool).await?;
    let bag_tiers = db::loot::list_bag_tiers(pool).await?;
    let threshold = db::loot::get_threshold(pool, run.guild_id).await?;

    let (embed, components) =
        embeds::run::build(run, &template, &emoji_map, &bag_tiers, &threshold);

    if let Err(e) = serenity::ChannelId::new(run.channel_id as u64)
        .edit_message(
            &ctx.http,
            MessageId::new(run.message_id as u64),
            EditMessage::new().add_embed(embed).components(components),
        )
        .await
    {
        if services::channels::is_not_found(&e) {
            tracing::warn!(
                run_id = run.id,
                channel_id = run.channel_id,
                message_id = run.message_id,
                "run message gone (404) on edit; auto-ending run to release \
                 slot claim and clean up VC immediately",
            );
            // `end_run` will try to edit the (now-gone) message to its ended
            // state and 404 again — best-effort, it'll log and continue.
            // Pass `None` for ended_by so the audit log credits the bot.
            if let Err(end_err) = services::raid::end_run(ctx, pool, run, None).await {
                tracing::warn!(
                    error = ?end_err,
                    run_id = run.id,
                    "auto-end after 404 failed; orphan sweep will clean up on next restart",
                );
            }
            return Ok(());
        }
        return Err(e.into());
    }

    Ok(())
}

/// Organizer gate: leader OR ManageRuns OR guild/global superadmin OR
/// Discord Manage Server. Returns true when authorized; false after sending
/// an ephemeral denial.
async fn require_organizer(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    run: &Run,
) -> Result<bool, BotError> {
    let ok = services::permission::can_organize_from_interaction(
        &data.db,
        run.guild_id,
        mci,
        run.leader_user_id,
        Some(run.tier_id),
        Some(run.dungeon_template_id),
    )
    .await?;
    if !ok {
        mci.create_response(
            ctx,
            ephemeral_msg("Only the raid leader or users with **ManageRuns** can do that."),
        )
        .await?;
    }
    Ok(ok)
}

// ---------------------------------------------------------------------------
// Control Panel
// ---------------------------------------------------------------------------

async fn handle_cp(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    run_id: i32,
) -> Result<(), BotError> {
    let Some(run) = load_active(ctx, mci, data, run_id).await? else {
        return Ok(());
    };
    if !require_organizer(ctx, mci, data, &run).await? {
        return Ok(());
    }

    let (embed, components) = embeds::run::control_panel(&run);

    mci.create_response(
        ctx,
        CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .add_embed(embed)
                .components(components)
                .ephemeral(true),
        ),
    )
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Location / Party modals
// ---------------------------------------------------------------------------

async fn handle_loc_open(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    run_id: i32,
) -> Result<(), BotError> {
    let Some(run) = load_active(ctx, mci, data, run_id).await? else {
        return Ok(());
    };
    if !require_organizer(ctx, mci, data, &run).await? {
        return Ok(());
    }

    let current = run.location.clone().unwrap_or_default();
    let modal = CreateModal::new(format!("run:{run_id}:loc"), "Set run location").components(vec![
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Short, "Location", "location")
                .placeholder("e.g. USW3 realm, nexus 5 o'clock")
                .value(current)
                .required(false)
                .max_length(200),
        ),
    ]);
    mci.create_response(ctx, CreateInteractionResponse::Modal(modal))
        .await?;
    Ok(())
}

async fn handle_party_open(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    run_id: i32,
) -> Result<(), BotError> {
    let Some(run) = load_active(ctx, mci, data, run_id).await? else {
        return Ok(());
    };
    if !require_organizer(ctx, mci, data, &run).await? {
        return Ok(());
    }

    let current = run.party.clone().unwrap_or_default();
    let modal = CreateModal::new(format!("run:{run_id}:party"), "Set party composition")
        .components(vec![CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Paragraph, "Party", "party")
                .placeholder("Free-form: classes, roles, pairings…")
                .value(current)
                .required(false)
                .max_length(1000),
        )]);
    mci.create_response(ctx, CreateInteractionResponse::Modal(modal))
        .await?;
    Ok(())
}

fn extract_input(modal: &serenity::ModalInteraction, custom_id: &str) -> Option<String> {
    for row in &modal.data.components {
        for comp in &row.components {
            if let ActionRowComponent::InputText(it) = comp {
                if it.custom_id == custom_id {
                    return it.value.clone();
                }
            }
        }
    }
    None
}

/// Modal submissions don't carry the same `ComponentInteraction` shape that
/// `can_organize_from_interaction` expects, so we delegate to the
/// modal-aware helper which refreshes the caller's role list against the
/// live guild member API (closes the 15-minute stale-token window).
///
/// Returns `Ok(None)` when the caller is no longer a member of the guild;
/// callers should treat that as a denial.
async fn modal_caller_is_organizer(
    ctx: &serenity::Context,
    pool: &sqlx::PgPool,
    modal: &serenity::ModalInteraction,
    run: &Run,
) -> Result<Option<bool>, BotError> {
    Ok(services::permission::can_organize_from_modal(
        ctx,
        pool,
        run.guild_id,
        modal,
        run.leader_user_id,
        Some(run.tier_id),
        Some(run.dungeon_template_id),
    )
    .await?)
}

async fn handle_loc_submit(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    data: &BotData,
    run_id: i32,
) -> Result<(), BotError> {
    let Some(run) = db::run::get(&data.db, run_id).await? else {
        modal
            .create_response(ctx, ephemeral_msg("This run has ended."))
            .await?;
        return Ok(());
    };
    match modal_caller_is_organizer(ctx, &data.db, modal, &run).await? {
        Some(true) => {}
        Some(false) => {
            modal
                .create_response(
                    ctx,
                    ephemeral_msg("Only the raid leader or users with **ManageRuns** can do that."),
                )
                .await?;
            return Ok(());
        }
        None => {
            modal
                .create_response(
                    ctx,
                    ephemeral_msg("You're no longer a member of this server."),
                )
                .await?;
            return Ok(());
        }
    }

    let raw = extract_input(modal, "location").unwrap_or_default();
    let trimmed = raw.trim();
    let new_loc: Option<&str> = if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    };
    db::run::set_location(&data.db, run_id, new_loc).await?;

    modal
        .create_response(
            ctx,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(if new_loc.is_some() {
                        "📍 Location updated."
                    } else {
                        "📍 Location cleared."
                    })
                    .ephemeral(true),
            ),
        )
        .await?;

    // Run may have been ended (concurrent End click, idle sweeper) between
    // the UPDATE above and this re-read. The user's ephemeral has already
    // posted, so swallow the rebuild — the public message is in its ended
    // state and there's nothing to refresh.
    let Some(refreshed) = db::run::get(&data.db, run_id).await? else {
        return Ok(());
    };
    rebuild_and_edit_message(ctx, data, &refreshed).await?;
    Ok(())
}

async fn handle_party_submit(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    data: &BotData,
    run_id: i32,
) -> Result<(), BotError> {
    let Some(run) = db::run::get(&data.db, run_id).await? else {
        modal
            .create_response(ctx, ephemeral_msg("This run has ended."))
            .await?;
        return Ok(());
    };
    match modal_caller_is_organizer(ctx, &data.db, modal, &run).await? {
        Some(true) => {}
        Some(false) => {
            modal
                .create_response(
                    ctx,
                    ephemeral_msg("Only the raid leader or users with **ManageRuns** can do that."),
                )
                .await?;
            return Ok(());
        }
        None => {
            modal
                .create_response(
                    ctx,
                    ephemeral_msg("You're no longer a member of this server."),
                )
                .await?;
            return Ok(());
        }
    }

    let raw = extract_input(modal, "party").unwrap_or_default();
    let trimmed = raw.trim();
    let new_party: Option<&str> = if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    };
    db::run::set_party(&data.db, run_id, new_party).await?;

    modal
        .create_response(
            ctx,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(if new_party.is_some() {
                        "📝 Party updated."
                    } else {
                        "📝 Party cleared."
                    })
                    .ephemeral(true),
            ),
        )
        .await?;

    let Some(refreshed) = db::run::get(&data.db, run_id).await? else {
        return Ok(());
    };
    rebuild_and_edit_message(ctx, data, &refreshed).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Transfer leader
// ---------------------------------------------------------------------------

async fn handle_transfer_open(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    run_id: i32,
) -> Result<(), BotError> {
    let Some(run) = load_active(ctx, mci, data, run_id).await? else {
        return Ok(());
    };
    if !require_organizer(ctx, mci, data, &run).await? {
        return Ok(());
    }

    let menu = CreateSelectMenu::new(
        format!("run:{run_id}:xfer"),
        CreateSelectMenuKind::User {
            default_users: None,
        },
    )
    .placeholder("Pick the new leader")
    .min_values(1)
    .max_values(1);

    mci.create_response(
        ctx,
        CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content("Choose a new leader.")
                .components(vec![CreateActionRow::SelectMenu(menu)])
                .ephemeral(true),
        ),
    )
    .await?;
    Ok(())
}

async fn handle_transfer_submit(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    run_id: i32,
) -> Result<(), BotError> {
    let Some(run) = load_active(ctx, mci, data, run_id).await? else {
        return Ok(());
    };
    if !require_organizer(ctx, mci, data, &run).await? {
        return Ok(());
    }

    let new_leader: i64 = match &mci.data.kind {
        serenity::ComponentInteractionDataKind::UserSelect { values } => match values.first() {
            Some(u) => u.get() as i64,
            None => return Ok(()),
        },
        _ => return Ok(()),
    };

    if new_leader == run.leader_user_id {
        mci.create_response(
            ctx,
            ephemeral_msg("That's already the leader — pick someone else."),
        )
        .await?;
        return Ok(());
    }

    // Update the run's leader and (when self-organize is in play) the
    // claim's leader together — the per-user cap follows the new owner so
    // the previous leader can immediately start another raid.
    let mut tx = data.db.begin().await?;
    db::run::set_leader_tx(&mut tx, run_id, new_leader).await?;
    if run.is_self_organized {
        db::self_organize::claim_set_leader(&mut tx, run_id, new_leader).await?;
    }
    tx.commit().await?;

    // Run may have been ended concurrently between the transfer commit and
    // this re-read. Skip the rebuild + listing refresh — there's nothing
    // left to display, and end_run already handled the public message.
    let Some(refreshed) = db::run::get(&data.db, run_id).await? else {
        mci.create_response(
            ctx,
            CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new()
                    .content("This run has ended.")
                    .components(vec![]),
            ),
        )
        .await?;
        return Ok(());
    };
    rebuild_and_edit_message(ctx, data, &refreshed).await?;

    // Refresh listing if self-organize, so the leader column updates.
    if run.is_self_organized {
        if let Ok(Some(tier)) = db::tier::get_by_id(&data.db, run.tier_id).await {
            if tier.enable_self_organization {
                if let Err(e) =
                    services::self_organize_listing::refresh_listing(ctx, &data.db, &tier).await
                {
                    tracing::warn!(
                        error = ?e,
                        tier_id = tier.id,
                        "failed to refresh self-organize listing after leader transfer",
                    );
                }
            }
        }
    }

    mci.create_response(
        ctx,
        CreateInteractionResponse::UpdateMessage(
            CreateInteractionResponseMessage::new()
                .content(format!("🔁 Leader transferred to <@{new_leader}>."))
                .components(vec![]),
        ),
    )
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// End run
// ---------------------------------------------------------------------------

async fn handle_end(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    run_id: i32,
) -> Result<(), BotError> {
    let Some(run) = load_active(ctx, mci, data, run_id).await? else {
        return Ok(());
    };
    if !require_organizer(ctx, mci, data, &run).await? {
        return Ok(());
    }

    // Delegate the teardown (claim release, row delete, VC delete, embed
    // edit, audit log, listing refresh) to the shared helper so this
    // handler and the periodic idle-timeout sweeper stay in lockstep.
    let ended = services::raid::end_run(ctx, &data.db, &run, Some(mci.user.id)).await?;
    if !ended {
        // Concurrent click already deleted the row. Tell the user.
        if let Err(e) = mci
            .create_response(ctx, ephemeral_msg("This run has ended."))
            .await
        {
            tracing::warn!(error = ?e, run_id, "failed to ack already-ended run");
        }
        return Ok(());
    }

    if let Err(e) = mci
        .create_response(
            ctx,
            CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new()
                    .content("🛑 Run ended.")
                    .embeds(vec![])
                    .components(vec![]),
            ),
        )
        .await
    {
        tracing::warn!(error = ?e, run_id = run.id, "failed to ack End to invoking user");
    }

    Ok(())
}
