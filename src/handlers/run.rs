//! Component + modal routing for `run:*` custom_ids. Keeps run lifecycle
//! plumbing out of `handlers/component.rs`, which already carries the
//! headcount flow.
//!
//! custom_id grammar (stateless, survives restarts):
//!   run:<id>:join                 -- anyone: add self as participant
//!   run:<id>:leave                -- anyone: remove self (all rows)
//!   run:<id>:cp                   -- anyone clicks; leader-only response
//!   run:<id>:loc                  -- leader: open location modal
//!   run:<id>:party                -- leader: open party modal
//!   run:<id>:transfer             -- leader: open UserSelect for new leader
//!   run:<id>:xfer                 -- submission of the UserSelect menu
//!   run:<id>:end                  -- leader: mark ended, lock the embed
//!   run:<id>:confirm:<rid>        -- anyone: ephemeral confirm for an item
//!   run:<id>:confirm_cancel       -- dismiss the ephemeral confirm prompt

use poise::serenity_prelude as serenity;
use serenity::{
    ActionRowComponent, ButtonStyle, CreateActionRow, CreateButton, CreateInputText,
    CreateInteractionResponse, CreateInteractionResponseMessage, CreateModal, CreateSelectMenu,
    CreateSelectMenuKind, EditMessage, InputTextStyle, MessageId, ReactionType,
};

use crate::db::models::Run;
use crate::{db, embeds, BotData, BotError};

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
        "join" => handle_join(ctx, mci, data, run_id).await,
        "leave" => handle_leave(ctx, mci, data, run_id).await,
        "cp" => handle_cp(ctx, mci, data, run_id).await,
        "loc" => handle_loc_open(ctx, mci, data, run_id).await,
        "party" => handle_party_open(ctx, mci, data, run_id).await,
        "transfer" => handle_transfer_open(ctx, mci, data, run_id).await,
        "xfer" => handle_transfer_submit(ctx, mci, data, run_id).await,
        "end" => handle_end(ctx, mci, data, run_id).await,
        "confirm" if parts.len() >= 4 => {
            let rid: i32 = match parts[3].parse() {
                Ok(n) => n,
                Err(_) => return Ok(()),
            };
            handle_confirm_click(ctx, mci, data, run_id, rid).await
        }
        "confirm_do" if parts.len() >= 4 => {
            let rid: i32 = match parts[3].parse() {
                Ok(n) => n,
                Err(_) => return Ok(()),
            };
            handle_confirm_do(ctx, mci, data, run_id, rid).await
        }
        "confirm_cancel" => handle_confirm_cancel(ctx, mci).await,
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

/// Rebuild the public run message from the current DB state. Used after any
/// participant / location / party / leader / status change.
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
    let participants = db::run::list_participants(pool, run.id).await?;
    let emoji_map = db::emoji::get_all_as_map(pool).await?;
    let tier = db::tier::get_by_id(pool, run.tier_id)
        .await?
        .ok_or_else(|| format!("tier {} not found", run.tier_id))?;

    let (embed, components) = embeds::run::build(
        run,
        &template,
        &reactions,
        &participants,
        &emoji_map,
        &tier.name,
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

async fn require_leader(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    run: &Run,
) -> Result<bool, BotError> {
    if (mci.user.id.get() as i64) == run.leader_user_id {
        return Ok(true);
    }
    mci.create_response(
        ctx,
        ephemeral_msg("Only the run leader can do that."),
    )
    .await?;
    Ok(false)
}

// ---------------------------------------------------------------------------
// Join / Leave
// ---------------------------------------------------------------------------

async fn handle_join(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    run_id: i32,
) -> Result<(), BotError> {
    let Some(run) = load_active(ctx, mci, data, run_id).await? else {
        return Ok(());
    };

    let user_id = mci.user.id.get() as i64;
    // "Join" creates the bare no-item row. If they already have any row
    // (item or not) this is a no-op at the SQL layer, so clicking Join
    // twice doesn't spam rows.
    db::run::add_participant(&data.db, run_id, user_id, None, false).await?;

    rebuild_and_edit_message(ctx, data, &run).await?;
    mci.create_response(ctx, ephemeral_msg("✅ You're in.")).await?;
    Ok(())
}

async fn handle_leave(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    run_id: i32,
) -> Result<(), BotError> {
    let Some(run) = load_active(ctx, mci, data, run_id).await? else {
        return Ok(());
    };

    let user_id = mci.user.id.get() as i64;
    // Leaders can't leave without transferring first — it would orphan
    // the run.
    if user_id == run.leader_user_id {
        mci.create_response(
            ctx,
            ephemeral_msg(
                "You're the leader — transfer the run from the Control Panel before leaving, \
                 or end the run instead.",
            ),
        )
        .await?;
        return Ok(());
    }

    db::run::remove_participant_all(&data.db, run_id, user_id).await?;

    rebuild_and_edit_message(ctx, data, &run).await?;
    mci.create_response(ctx, ephemeral_msg("👋 Left the run.")).await?;
    Ok(())
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

    if !require_leader(ctx, mci, &run).await? {
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
    if !require_leader(ctx, mci, &run).await? {
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
    if !require_leader(ctx, mci, &run).await? {
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
    if (modal.user.id.get() as i64) != run.leader_user_id {
        modal.create_response(ctx, ephemeral_msg("Only the run leader can do that.")).await?;
        return Ok(());
    }

    let raw = extract_input(modal, "location").unwrap_or_default();
    let trimmed = raw.trim();
    let new_loc: Option<&str> = if trimmed.is_empty() { None } else { Some(trimmed) };
    db::run::set_location(&data.db, run_id, new_loc).await?;

    // Acknowledge first so the modal closes, then edit the public message
    // out of band (modal submissions can't UpdateMessage another message).
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
    if (modal.user.id.get() as i64) != run.leader_user_id {
        modal.create_response(ctx, ephemeral_msg("Only the run leader can do that.")).await?;
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
    if !require_leader(ctx, mci, &run).await? {
        return Ok(());
    }

    // UserSelect menu. The current leader should pick the new one; Discord
    // doesn't let us exclude a specific user client-side, but the submit
    // handler rejects self-transfer.
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
                .content(
                    "Choose a new leader. They don't have to be in the run yet — they \
                     will be auto-joined on transfer.",
                )
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
    if !require_leader(ctx, mci, &run).await? {
        return Ok(());
    }

    // Unpack the selected user from the interaction.
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
    // Auto-join the new leader if they weren't already on the run.
    db::run::add_participant(&data.db, run_id, new_leader, None, false).await?;

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
    if !require_leader(ctx, mci, &run).await? {
        return Ok(());
    }

    db::run::set_status(&data.db, run_id, "ended").await?;

    // Rebuild the public message as the ended (greyed, no components) embed.
    let pool = &data.db;
    let template = db::dungeon::get_by_id(pool, run.dungeon_template_id)
        .await?
        .ok_or_else(|| format!("template {} not found", run.dungeon_template_id))?;
    let reactions = db::dungeon::get_reactions(pool, run.dungeon_template_id).await?;
    let participants = db::run::list_participants(pool, run.id).await?;
    let emoji_map = db::emoji::get_all_as_map(pool).await?;
    let tier = db::tier::get_by_id(pool, run.tier_id)
        .await?
        .ok_or_else(|| format!("tier {} not found", run.tier_id))?;

    let ended_embed = embeds::run::build_ended(
        &run,
        &template,
        &reactions,
        &participants,
        &emoji_map,
        &tier.name,
    );

    serenity::ChannelId::new(run.channel_id as u64)
        .edit_message(
            &ctx.http,
            MessageId::new(run.message_id as u64),
            EditMessage::new().add_embed(ended_embed).components(vec![]),
        )
        .await?;

    // Close the control-panel ephemeral (the only click that can reach here).
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

    // Best-effort log: drop a follow-up in the log channel if configured.
    if let Ok(Some(guild)) = db::guild::get(pool, run.guild_id).await {
        if let Some(log_id) = guild.log_channel_id {
            let _ = serenity::ChannelId::new(log_id as u64)
                .send_message(
                    &ctx.http,
                    serenity::CreateMessage::new().content(format!(
                        "Run #{id} ({name}) ended by <@{leader}>.",
                        id = run.id,
                        name = template.display_name,
                        leader = mci.user.id.get(),
                    )),
                )
                .await;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Per-item confirm flow (keys, runes, etc.)
// ---------------------------------------------------------------------------

async fn handle_confirm_click(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    run_id: i32,
    reaction_id: i32,
) -> Result<(), BotError> {
    let Some(run) = load_active(ctx, mci, data, run_id).await? else {
        return Ok(());
    };

    let reactions = db::dungeon::get_reactions(&data.db, run.dungeon_template_id).await?;
    let Some(reaction) = reactions.iter().find(|r| r.id == reaction_id) else {
        mci.create_response(ctx, ephemeral_msg("Unknown reaction.")).await?;
        return Ok(());
    };

    let confirm = CreateButton::new(format!("run:{run_id}:confirm_do:{reaction_id}"))
        .label("Confirm")
        .emoji(ReactionType::Unicode("✅".into()))
        .style(ButtonStyle::Success);
    let cancel = CreateButton::new(format!("run:{run_id}:confirm_cancel"))
        .label("Cancel")
        .style(ButtonStyle::Secondary);

    mci.create_response(
        ctx,
        CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content(format!(
                    "Confirm you're bringing **{}**? Only click Confirm if you actually have it.",
                    reaction.display_name
                ))
                .components(vec![CreateActionRow::Buttons(vec![confirm, cancel])])
                .ephemeral(true),
        ),
    )
    .await?;
    Ok(())
}

async fn handle_confirm_do(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    run_id: i32,
    reaction_id: i32,
) -> Result<(), BotError> {
    let Some(run) = load_active(ctx, mci, data, run_id).await? else {
        return Ok(());
    };

    let user_id = mci.user.id.get() as i64;
    db::run::add_participant(&data.db, run_id, user_id, Some(reaction_id), true).await?;

    rebuild_and_edit_message(ctx, data, &run).await?;

    mci.create_response(
        ctx,
        CreateInteractionResponse::UpdateMessage(
            CreateInteractionResponseMessage::new()
                .content("✅ Confirmed!")
                .components(vec![]),
        ),
    )
    .await?;
    Ok(())
}

async fn handle_confirm_cancel(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
) -> Result<(), BotError> {
    mci.create_response(
        ctx,
        CreateInteractionResponse::UpdateMessage(
            CreateInteractionResponseMessage::new()
                .content("No changes made.")
                .components(vec![]),
        ),
    )
    .await?;
    Ok(())
}
