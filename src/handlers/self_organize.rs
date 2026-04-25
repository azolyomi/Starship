//! Component + modal handling for `so:*` (self-organize) custom_ids.
//!
//! Click flow:
//!
//!   so:btn:<tier_id>                       sticky button on the tier's
//!                                          self-organize channel — opens an
//!                                          ephemeral dungeon picker (page 0).
//!   so:page:<tier_id>:<page>               Prev/Next click on the picker —
//!                                          re-renders the ephemeral with
//!                                          a different slice of the tier's
//!                                          dungeons. Ephemeral-only.
//!   so:dpick:<tier_id>                     StringSelect submission — opens
//!                                          a location/party modal whose ID
//!                                          encodes the chosen dungeon.
//!   so:start:<tier_id>:<template_id>       modal submit — runs the
//!                                          anti-troll gate, calls
//!                                          `start_headcount_inner` with
//!                                          `is_self_organized = true`, and
//!                                          refreshes the listing.
//!
//! Permissions are intentionally absent: any guild member who can click the
//! button can start a raid. The anti-troll gate (slot lock, per-user cap,
//! cooldown) is what holds back abuse.
//!
//! Each step is independently routable via the stateless custom_id, so a
//! click against an old ephemeral after a bot restart still works.

use poise::serenity_prelude as serenity;
use serenity::{
    ActionRowComponent, ButtonStyle, CreateActionRow, CreateButton, CreateInputText,
    CreateInteractionResponse, CreateInteractionResponseFollowup, CreateInteractionResponseMessage,
    CreateModal, CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption, InputTextStyle,
};
use sqlx::PgPool;

use crate::db::models::Tier;
use crate::services::raid::StartHeadcountOutcome;
use crate::services::{self_organize, self_organize_listing};
use crate::{db, services, BotData, BotError};

/// One page of the dungeon picker. Discord's hard cap on StringSelect
/// options is 25; pagination beyond that uses Prev/Next nav buttons.
const PICKER_PAGE_SIZE: usize = 25;

// ---------------------------------------------------------------------------
// Top-level dispatchers
// ---------------------------------------------------------------------------

pub async fn handle_component(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
) -> Result<(), BotError> {
    let parts: Vec<&str> = mci.data.custom_id.split(':').collect();
    // Grammar: so:<action>:<tier_id>[...]
    if parts.len() < 3 {
        return Ok(());
    }
    let Ok(tier_id) = parts[2].parse::<i32>() else {
        return Ok(());
    };
    match parts[1] {
        "btn" => handle_button(ctx, mci, data, tier_id).await,
        "page" => {
            // so:page:<tier_id>:<page>
            let page: usize = parts.get(3).and_then(|p| p.parse().ok()).unwrap_or(0);
            handle_page(ctx, mci, data, tier_id, page).await
        }
        "dpick" => handle_dpick(ctx, mci, data, tier_id).await,
        _ => Ok(()),
    }
}

pub async fn handle_modal(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    data: &BotData,
) -> Result<(), BotError> {
    let parts: Vec<&str> = modal.data.custom_id.split(':').collect();
    // so:start:<tier_id>:<template_id>
    if parts.len() < 4 || parts[1] != "start" {
        return Ok(());
    }
    let Ok(tier_id) = parts[2].parse::<i32>() else {
        return Ok(());
    };
    let Ok(template_id) = parts[3].parse::<i32>() else {
        return Ok(());
    };
    handle_start(ctx, modal, data, tier_id, template_id).await
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ephemeral_msg(text: impl Into<String>) -> CreateInteractionResponse {
    CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new()
            .content(text)
            .ephemeral(true),
    )
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

async fn load_tier_and_check_enabled(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    tier_id: i32,
) -> Result<Option<Tier>, BotError> {
    let Some(tier) = db::tier::get_by_id(&data.db, tier_id).await? else {
        mci.create_response(
            ctx,
            ephemeral_msg(
                "This tier no longer exists. The button will be cleaned up on the next restart.",
            ),
        )
        .await?;
        return Ok(None);
    };
    if !tier.enable_self_organization {
        mci.create_response(
            ctx,
            ephemeral_msg(
                "Self-organized raids are no longer enabled for this tier. \
                 Ask a server admin to re-enable in `/setup` if you'd like to use it.",
            ),
        )
        .await?;
        return Ok(None);
    }
    Ok(Some(tier))
}

// ---------------------------------------------------------------------------
// Step 1: sticky button click → dungeon picker (page 0)
// ---------------------------------------------------------------------------

async fn handle_button(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    tier_id: i32,
) -> Result<(), BotError> {
    let Some(tier) = load_tier_and_check_enabled(ctx, mci, data, tier_id).await? else {
        return Ok(());
    };

    let rendered = match render_picker_page(&data.db, &tier, 0).await? {
        Some(r) => r,
        None => {
            mci.create_response(
                ctx,
                ephemeral_msg(
                    "This tier has no dungeons configured yet. Ask a server admin \
                     to attach dungeons in `/tier add-dungeon`.",
                ),
            )
            .await?;
            return Ok(());
        }
    };

    mci.create_response(
        ctx,
        CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content(rendered.content)
                .components(rendered.components)
                .ephemeral(true),
        ),
    )
    .await?;
    Ok(())
}

/// Prev/Next click on an open picker ephemeral. Re-renders the same
/// ephemeral with a different slice of the tier's dungeons.
async fn handle_page(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    tier_id: i32,
    page: usize,
) -> Result<(), BotError> {
    let Some(tier) = load_tier_and_check_enabled(ctx, mci, data, tier_id).await? else {
        return Ok(());
    };

    let rendered = match render_picker_page(&data.db, &tier, page).await? {
        Some(r) => r,
        None => {
            // Tier emptied between the original click and this nav click —
            // dismiss the picker with a plain message.
            mci.create_response(
                ctx,
                CreateInteractionResponse::UpdateMessage(
                    CreateInteractionResponseMessage::new()
                        .content(
                            "This tier has no dungeons configured anymore. Ask a server admin \
                             to attach dungeons in `/tier add-dungeon`.",
                        )
                        .components(vec![]),
                ),
            )
            .await?;
            return Ok(());
        }
    };

    mci.create_response(
        ctx,
        CreateInteractionResponse::UpdateMessage(
            CreateInteractionResponseMessage::new()
                .content(rendered.content)
                .components(rendered.components),
        ),
    )
    .await?;
    Ok(())
}

struct PickerPage {
    content: String,
    components: Vec<CreateActionRow>,
}

/// Build one page of the picker. Returns `None` when the tier has no
/// dungeons attached; the caller picks the right user-facing wording for
/// the entry point (first-click vs. nav-click).
async fn render_picker_page(
    pool: &PgPool,
    tier: &Tier,
    page: usize,
) -> Result<Option<PickerPage>, BotError> {
    let template_ids = db::tier::list_dungeons(pool, tier.id).await?;
    if template_ids.is_empty() {
        return Ok(None);
    }

    let total_pages = template_ids.len().div_ceil(PICKER_PAGE_SIZE).max(1);
    let page = page.min(total_pages - 1);
    let start = page * PICKER_PAGE_SIZE;
    let end = (start + PICKER_PAGE_SIZE).min(template_ids.len());

    let mut options: Vec<CreateSelectMenuOption> = Vec::with_capacity(end - start);
    for tid in &template_ids[start..end] {
        // Resolve per-id; tier sizes are bounded (one page = 25 max) so
        // the per-row query is fine. The alternative is a custom join
        // helper that's only used here.
        let Some(template) = db::dungeon::get_by_id(pool, *tid).await? else {
            continue;
        };
        // Encode the template_id directly as the select value so we don't
        // round-trip through name-resolution when the user picks.
        let mut opt = CreateSelectMenuOption::new(template.display_name, template.id.to_string());
        if let Some(emoji) = template.emoji.as_deref() {
            // Only attach unicode emoji literals — custom application
            // emoji would need the bot_emoji map and a ReactionType build
            // that's overkill for cosmetic flair.
            if !emoji.is_ascii() {
                opt = opt.emoji(serenity::ReactionType::Unicode(emoji.to_string()));
            }
        }
        options.push(opt);
    }

    let menu = CreateSelectMenu::new(
        format!("so:dpick:{}", tier.id),
        CreateSelectMenuKind::String { options },
    )
    .placeholder("Pick a dungeon")
    .min_values(1)
    .max_values(1);

    let mut components = vec![CreateActionRow::SelectMenu(menu)];

    if total_pages > 1 {
        let prev = CreateButton::new(format!("so:page:{}:{}", tier.id, page.saturating_sub(1)))
            .label("\u{2190} Prev")
            .style(ButtonStyle::Secondary)
            .disabled(page == 0);
        let next = CreateButton::new(format!("so:page:{}:{}", tier.id, page + 1))
            .label("Next \u{2192}")
            .style(ButtonStyle::Secondary)
            .disabled(page + 1 >= total_pages);
        components.push(CreateActionRow::Buttons(vec![prev, next]));
    }

    let pagination_note = if total_pages > 1 {
        format!(" \u{00B7} Page {} / {}", page + 1, total_pages)
    } else {
        String::new()
    };

    let content = format!(
        "Pick a dungeon to start a headcount in **{}**.{pagination_note}",
        tier.name,
    );

    Ok(Some(PickerPage {
        content,
        components,
    }))
}

// ---------------------------------------------------------------------------
// Step 2: dungeon picked → location/party modal
// ---------------------------------------------------------------------------

async fn handle_dpick(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    tier_id: i32,
) -> Result<(), BotError> {
    let Some(_tier) = load_tier_and_check_enabled(ctx, mci, data, tier_id).await? else {
        return Ok(());
    };

    let template_id: i32 = match &mci.data.kind {
        serenity::ComponentInteractionDataKind::StringSelect { values } => {
            match values.first().and_then(|v| v.parse().ok()) {
                Some(id) => id,
                None => return Ok(()),
            }
        }
        _ => return Ok(()),
    };

    // Verify the template still exists before we open the modal — the user
    // would otherwise type out their location/party only to see a "dungeon
    // not found" error on submit.
    if db::dungeon::get_by_id(&data.db, template_id)
        .await?
        .is_none()
    {
        mci.create_response(
            ctx,
            ephemeral_msg("That dungeon is no longer available. Try opening the picker again."),
        )
        .await?;
        return Ok(());
    }

    let modal = CreateModal::new(
        format!("so:start:{tier_id}:{template_id}"),
        "Start a headcount",
    )
    .components(vec![
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Short, "Location", "location")
                .placeholder("e.g. USW3 realm, nexus 5 o'clock")
                .required(false)
                .max_length(200),
        ),
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Paragraph, "Party", "party")
                .placeholder("Free-form: classes, roles, pairings\u{2026}")
                .required(false)
                .max_length(1000),
        ),
    ]);
    mci.create_response(ctx, CreateInteractionResponse::Modal(modal))
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 3: modal submit → gate + create + refresh listing
// ---------------------------------------------------------------------------

async fn handle_start(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    data: &BotData,
    tier_id: i32,
    template_id: i32,
) -> Result<(), BotError> {
    // Defer immediately: posting the HC + attaching reactions can take
    // several seconds and would otherwise blow Discord's 3s interaction
    // window.
    modal
        .create_response(
            ctx,
            CreateInteractionResponse::Defer(
                CreateInteractionResponseMessage::new().ephemeral(true),
            ),
        )
        .await?;

    let pool = &data.db;

    let Some(tier) = db::tier::get_by_id(pool, tier_id).await? else {
        modal
            .create_followup(
                ctx,
                CreateInteractionResponseFollowup::new()
                    .content("This tier no longer exists.")
                    .ephemeral(true),
            )
            .await?;
        return Ok(());
    };
    if !tier.enable_self_organization {
        modal
            .create_followup(
                ctx,
                CreateInteractionResponseFollowup::new()
                    .content("Self-organized raids are no longer enabled for this tier.")
                    .ephemeral(true),
            )
            .await?;
        return Ok(());
    }

    let Some(template) = db::dungeon::get_by_id(pool, template_id).await? else {
        modal
            .create_followup(
                ctx,
                CreateInteractionResponseFollowup::new()
                    .content("That dungeon is no longer available.")
                    .ephemeral(true),
            )
            .await?;
        return Ok(());
    };

    let Some(channel_id) = tier.runs_channel_id else {
        modal
            .create_followup(
                ctx,
                CreateInteractionResponseFollowup::new()
                    .content(format!(
                        "Tier **{}** has no runs channel. Ask an admin to set one in `/setup`.",
                        tier.name,
                    ))
                    .ephemeral(true),
            )
            .await?;
        return Ok(());
    };

    if !services::channels::channel_exists(&ctx.http, serenity::ChannelId::new(channel_id as u64))
        .await?
    {
        modal
            .create_followup(
                ctx,
                CreateInteractionResponseFollowup::new()
                    .content(format!(
                        "Tier **{}**'s runs channel <#{channel_id}> is gone. \
                         Ask an admin to repoint the tier in `/setup` or `/tier edit`.",
                        tier.name,
                    ))
                    .ephemeral(true),
            )
            .await?;
        return Ok(());
    }

    let caller_id = modal.user.id.get() as i64;

    // Anti-troll gate. Runs *outside* the tx because it does its own
    // best-effort sweep of stale claims (which itself opens a tx) and
    // Postgres doesn't allow nested tx without savepoints.
    if let Some(block) =
        self_organize::check_can_start(ctx, pool, &tier, &template, caller_id).await?
    {
        modal
            .create_followup(
                ctx,
                CreateInteractionResponseFollowup::new()
                    .content(block.user_message())
                    .ephemeral(true),
            )
            .await?;
        return Ok(());
    }

    let location_raw = extract_input(modal, "location").unwrap_or_default();
    let location_trim = location_raw.trim();
    let location: Option<&str> = if location_trim.is_empty() {
        None
    } else {
        Some(location_trim)
    };
    let party_raw = extract_input(modal, "party").unwrap_or_default();
    let party_trim = party_raw.trim();
    let party: Option<&str> = if party_trim.is_empty() {
        None
    } else {
        Some(party_trim)
    };

    let outcome = services::raid::start_headcount_inner(
        ctx,
        pool,
        tier.guild_id,
        caller_id,
        &tier,
        &template,
        channel_id,
        true, // is_self_organized
    )
    .await?;

    match outcome {
        StartHeadcountOutcome::Started(hc) => {
            // Stash modal-supplied location/party on the row so the
            // HC->Run convert modal pre-fills. The headcount embed itself
            // doesn't render these — they're a leader-private prefill.
            if location.is_some() || party.is_some() {
                if let Err(e) =
                    db::headcount::set_location_and_party(pool, hc.id, location, party).await
                {
                    tracing::warn!(
                        error = ?e,
                        hc_id = hc.id,
                        "failed to persist self-organize HC location/party",
                    );
                }
            }

            // Refresh the listing best-effort — the HC is already up, so a
            // listing failure is purely cosmetic and shouldn't bubble.
            if let Err(e) = self_organize_listing::refresh_listing(ctx, pool, &tier).await {
                tracing::warn!(
                    error = ?e,
                    tier_id = tier.id,
                    "failed to refresh self-organize listing after start",
                );
            }

            modal
                .create_followup(
                    ctx,
                    CreateInteractionResponseFollowup::new()
                        .content(format!(
                            "Headcount started in <#{channel_id}>! React on the message \
                             to sign up; click **Start Run** when you're ready."
                        ))
                        .ephemeral(true),
                )
                .await?;
        }
        StartHeadcountOutcome::SlotInUse(holder) => {
            // Race: the slot was claimed between our gate check and the tx.
            // Render the standard SlotInUse message.
            modal
                .create_followup(
                    ctx,
                    CreateInteractionResponseFollowup::new()
                        .content(self_organize::SelfOrganizeBlock::SlotInUse(holder).user_message())
                        .ephemeral(true),
                )
                .await?;
        }
    }

    Ok(())
}
