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
            mci.create_response(ctx, ephemeral_msg("Run not found.")).await?;
            Ok(None)
        }
        Some(run) if run.status != "active" => {
            mci.create_response(ctx, ephemeral_msg("This run has ended.")).await?;
            Ok(None)
        }
        Some(run) => Ok(Some(run)),
    }
}

/// Rebuild the public run message from the current DB state. Used after the
/// location, party, or leader changes.
async fn rebuild_and_edit_message(
    ctx: &serenity::Context,
    data: &BotData,
    run: &Run,
) -> Result<(), BotError> {
    let pool = &data.db;
    let template = db::dungeon::get_by_id(pool, run.dungeon_template_id)
        .await?
        .ok_or_else(|| format!("template {} not found", run.dungeon_template_id))?;
    let reactions = db::dungeon::get_reactions(pool, run.dungeon_template_id).await?;
    let emoji_map = db::emoji::get_all_as_map(pool).await?;
    let bag_tiers = db::loot::list_bag_tiers(pool).await?;
    let threshold = db::loot::get_threshold(pool, run.guild_id).await?;

    let (embed, components) = embeds::run::build(
        run,
        &template,
        &reactions,
        &emoji_map,
        &bag_tiers,
        &threshold,
    );

    serenity::ChannelId::new(run.channel_id as u64)
        .edit_message(
            &ctx.http,
            MessageId::new(run.message_id as u64),
            EditMessage::new().add_embed(embed).components(components),
        )
        .await?;

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
            ephemeral_msg(
                "Only the raid leader or users with **ManageRuns** can do that.",
            ),
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
    let modal = CreateModal::new(format!("run:{run_id}:loc"), "Set run location")
        .components(vec![CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Short, "Location", "location")
                .placeholder("e.g. USW3 realm, nexus 5 o'clock")
                .value(current)
                .required(false)
                .max_length(200),
        )]);
    mci.create_response(ctx, CreateInteractionResponse::Modal(modal)).await?;
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
    mci.create_response(ctx, CreateInteractionResponse::Modal(modal)).await?;
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
/// `can_organize_from_interaction` expects, so we reconstruct the gate from
/// the modal's member instead.
async fn modal_caller_is_organizer(
    pool: &sqlx::PgPool,
    modal: &serenity::ModalInteraction,
    run: &Run,
) -> Result<bool, BotError> {
    let caller_id = modal.user.id.get() as i64;
    let (perms, roles) = match modal.member.as_ref() {
        Some(m) => (m.permissions, m.roles.iter().map(|r| r.get() as i64).collect()),
        None => (None, Vec::new()),
    };
    Ok(services::permission::can_organize(
        pool,
        run.guild_id,
        caller_id,
        perms,
        &roles,
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
        modal.create_response(ctx, ephemeral_msg("Run not found.")).await?;
        return Ok(());
    };
    if run.status != "active" {
        modal.create_response(ctx, ephemeral_msg("This run has ended.")).await?;
        return Ok(());
    }
    if !modal_caller_is_organizer(&data.db, modal, &run).await? {
        modal
            .create_response(
                ctx,
                ephemeral_msg(
                    "Only the raid leader or users with **ManageRuns** can do that.",
                ),
            )
            .await?;
        return Ok(());
    }

    let raw = extract_input(modal, "location").unwrap_or_default();
    let trimmed = raw.trim();
    let new_loc: Option<&str> = if trimmed.is_empty() { None } else { Some(trimmed) };
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

    let refreshed = db::run::get(&data.db, run_id)
        .await?
        .expect("run existed a moment ago");
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
        modal.create_response(ctx, ephemeral_msg("Run not found.")).await?;
        return Ok(());
    };
    if run.status != "active" {
        modal.create_response(ctx, ephemeral_msg("This run has ended.")).await?;
        return Ok(());
    }
    if !modal_caller_is_organizer(&data.db, modal, &run).await? {
        modal
            .create_response(
                ctx,
                ephemeral_msg(
                    "Only the raid leader or users with **ManageRuns** can do that.",
                ),
            )
            .await?;
        return Ok(());
    }

    let raw = extract_input(modal, "party").unwrap_or_default();
    let trimmed = raw.trim();
    let new_party: Option<&str> = if trimmed.is_empty() { None } else { Some(trimmed) };
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

    let refreshed = db::run::get(&data.db, run_id)
        .await?
        .expect("run existed a moment ago");
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
        CreateSelectMenuKind::User { default_users: None },
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
        serenity::ComponentInteractionDataKind::UserSelect { values } => {
            match values.first() {
                Some(u) => u.get() as i64,
                None => return Ok(()),
            }
        }
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

    db::run::set_leader(&data.db, run_id, new_leader).await?;

    let refreshed = db::run::get(&data.db, run_id)
        .await?
        .expect("run existed a moment ago");
    rebuild_and_edit_message(ctx, data, &refreshed).await?;

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

    db::run::set_status(&data.db, run_id, "ended").await?;

    // Tear down the temp VC if this run had one. Best-effort — by the time
    // we get here the run is already marked ended; a dangling channel is a
    // manual-cleanup problem, not a flow-failure problem.
    if let Some(vc_id) = run.voice_channel_id {
        services::voice::delete_temp_vc(
            &ctx.http,
            serenity::ChannelId::new(vc_id as u64),
        )
        .await;
    }

    let pool = &data.db;
    let template = db::dungeon::get_by_id(pool, run.dungeon_template_id)
        .await?
        .ok_or_else(|| format!("template {} not found", run.dungeon_template_id))?;
    let emoji_map = db::emoji::get_all_as_map(pool).await?;

    let ended_embed = embeds::run::build_ended(&run, &template, &emoji_map);

    serenity::ChannelId::new(run.channel_id as u64)
        .edit_message(
            &ctx.http,
            MessageId::new(run.message_id as u64),
            EditMessage::new().add_embed(ended_embed).components(vec![]),
        )
        .await?;

    mci.create_response(
        ctx,
        CreateInteractionResponse::UpdateMessage(
            CreateInteractionResponseMessage::new()
                .content("🛑 Run ended.")
                .embeds(vec![])
                .components(vec![]),
        ),
    )
    .await?;

    // Best-effort audit log entry.
    if let Ok(Some(guild)) = db::guild::get(pool, run.guild_id).await {
        if let Some(log_id) = guild.log_channel_id {
            let _ = serenity::ChannelId::new(log_id as u64)
                .send_message(
                    &ctx.http,
                    serenity::CreateMessage::new().content(format!(
                        "Run #{id} ({name}) ended by <@{caller}>.",
                        id = run.id,
                        name = template.display_name,
                        caller = mci.user.id.get(),
                    )),
                )
                .await;
        }
    }

    Ok(())
}
