//! Component + modal handling for `de:*` (dungeon-edit) custom_ids.
//!
//! Both `/dungeon create` and `/dungeon edit` end up rendering the same
//! ephemeral on a real DB row. Each component click writes to the row
//! directly — there's no draft state and no in-memory session, so a
//! click on a stale ephemeral after a bot restart still works.
//!
//! Custom-ID grammar (all `de:` prefixed; routed in handlers::component
//! and handlers::modal):
//!
//!   de:react:<template_id>:<category>      reactions multi-select submit
//!                                          (category ∈ key / status_effect / class)
//!   de:tiers:<template_id>                 tier multi-select submit
//!   de:scalars:<template_id>               button — opens scalars modal
//!   de:scalarsubmit:<template_id>          modal submit (display_name + description + color)
//!   de:vc:<template_id>                    button — toggles requires_vc immediately
//!   de:tune:<template_id>                  button — opens tune-reactions sub-ephemeral
//!   de:tunepick:<reaction_id>              button — opens per-reaction modal
//!   de:tunesubmit:<reaction_id>            modal submit (reaction tuning)
//!   de:back:<template_id>                  button — back from tune to main
//!   de:done:_                              button — close ephemeral
//!   de:create:<inherit_or_0>               modal submit (from /dungeon create)
//!
//! The `<inherit_or_0>` token encodes the optional inherit-from source
//! template id (0 = none) so the modal handler can copy reactions in the
//! same INSERT transaction.

use std::collections::{HashMap, HashSet};

use poise::serenity_prelude as serenity;
use serenity::{
    ActionRowComponent, ButtonStyle, ComponentInteractionDataKind, CreateActionRow, CreateButton,
    CreateInputText, CreateInteractionResponse, CreateInteractionResponseMessage, CreateModal,
    CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption, InputTextStyle,
};
use sqlx::PgPool;

use crate::db::models::{BotEmoji, DungeonReaction, DungeonTemplate};
use crate::embeds::headcount::emoji_rt;
use crate::util::text::{slug_from_display, snake_to_title};
use crate::{db, limits, BotData, BotError};

/// Reaction categories surfaced as separate multi-selects in the edit
/// ephemeral. Order matters: it controls row order in the rendered UI.
/// `interest` isn't here because it's the implicit ✅ reaction every
/// guild-specific template carries; it isn't user-toggleable.
const REACTION_CATEGORIES: &[(&str, &str)] = &[
    ("key", "Keys"),
    ("status_effect", "Status"),
    ("class", "Classes"),
];

// ---------------------------------------------------------------------------
// Top-level dispatchers
// ---------------------------------------------------------------------------

pub async fn handle_component(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
) -> Result<(), BotError> {
    let parts: Vec<&str> = mci.data.custom_id.split(':').collect();
    if parts.len() < 2 {
        return Ok(());
    }
    match parts[1] {
        "react" if parts.len() >= 4 => {
            let Ok(template_id) = parts[2].parse::<i32>() else {
                return Ok(());
            };
            let category = parts[3];
            handle_reactions_submit(ctx, mci, data, template_id, category).await
        }
        "tiers" if parts.len() >= 3 => {
            let Ok(template_id) = parts[2].parse::<i32>() else {
                return Ok(());
            };
            handle_tiers_submit(ctx, mci, data, template_id).await
        }
        "scalars" if parts.len() >= 3 => {
            let Ok(template_id) = parts[2].parse::<i32>() else {
                return Ok(());
            };
            handle_scalars_button(ctx, mci, data, template_id).await
        }
        "vc" if parts.len() >= 3 => {
            let Ok(template_id) = parts[2].parse::<i32>() else {
                return Ok(());
            };
            handle_vc_toggle(ctx, mci, data, template_id).await
        }
        "tune" if parts.len() >= 3 => {
            let Ok(template_id) = parts[2].parse::<i32>() else {
                return Ok(());
            };
            handle_tune_open(ctx, mci, data, template_id).await
        }
        "tunepick" if parts.len() >= 3 => {
            let Ok(reaction_id) = parts[2].parse::<i32>() else {
                return Ok(());
            };
            handle_tune_pick(ctx, mci, data, reaction_id).await
        }
        "back" if parts.len() >= 3 => {
            let Ok(template_id) = parts[2].parse::<i32>() else {
                return Ok(());
            };
            handle_back(ctx, mci, data, template_id).await
        }
        "done" => handle_done(ctx, mci).await,
        _ => Ok(()),
    }
}

pub async fn handle_modal(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    data: &BotData,
) -> Result<(), BotError> {
    let parts: Vec<&str> = modal.data.custom_id.split(':').collect();
    if parts.len() < 2 {
        return Ok(());
    }
    match parts[1] {
        "create" if parts.len() >= 3 => {
            let inherit: Option<i32> = parts[2].parse::<i32>().ok().filter(|v| *v > 0);
            handle_create_submit(ctx, modal, data, inherit).await
        }
        "scalarsubmit" if parts.len() >= 3 => {
            let Ok(template_id) = parts[2].parse::<i32>() else {
                return Ok(());
            };
            handle_scalars_submit(ctx, modal, data, template_id).await
        }
        "tunesubmit" if parts.len() >= 3 => {
            let Ok(reaction_id) = parts[2].parse::<i32>() else {
                return Ok(());
            };
            handle_tune_submit(ctx, modal, data, reaction_id).await
        }
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Public entry points used by /dungeon create and /dungeon edit
// ---------------------------------------------------------------------------

/// Modal opened as the first response to `/dungeon create`. Collects
/// display_name + description + color hex; the rest of the create flow
/// runs in the modal submit handler.
pub fn build_create_modal(inherit_source: Option<&DungeonTemplate>) -> CreateModal {
    let inherit_id = inherit_source.map(|s| s.id).unwrap_or(0);
    let title = match inherit_source {
        Some(s) => format!("Create dungeon (from {})", s.display_name),
        None => "Create dungeon".to_string(),
    };
    let preset_description = inherit_source
        .and_then(|s| s.message_description.clone())
        .unwrap_or_default();
    let preset_color = inherit_source
        .and_then(|s| s.color)
        .map(|c| format!("{c:06X}"))
        .unwrap_or_default();
    CreateModal::new(format!("de:create:{inherit_id}"), title).components(vec![
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Short, "Display name", "display_name")
                .placeholder("e.g. Lost Halls Speedrun")
                .required(true)
                .max_length(limits::DISPLAY_NAME_MAX as u16),
        ),
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Paragraph, "Description (optional)", "description")
                .placeholder("Shown in the headcount embed")
                .value(preset_description)
                .required(false)
                .max_length(limits::DESCRIPTION_MAX as u16),
        ),
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Short, "Embed color hex (optional)", "color")
                .placeholder("e.g. FF4500")
                .value(preset_color)
                .required(false)
                .max_length(7),
        ),
    ])
}

/// Build the unified edit ephemeral as a `Message` response — used after
/// `/dungeon create`'s modal submit and `/dungeon edit`'s direct entry.
pub async fn build_edit_response(
    pool: &PgPool,
    template_id: i32,
    intro_note: Option<&str>,
) -> Result<CreateInteractionResponseMessage, BotError> {
    let view = render_edit_view(pool, template_id, intro_note).await?;
    Ok(CreateInteractionResponseMessage::new()
        .content(view.content)
        .components(view.components)
        .ephemeral(true))
}

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

struct EditView {
    content: String,
    components: Vec<CreateActionRow>,
}

async fn render_edit_view(
    pool: &PgPool,
    template_id: i32,
    intro_note: Option<&str>,
) -> Result<EditView, BotError> {
    let Some(template) = db::dungeon::get_by_id(pool, template_id).await? else {
        return Ok(EditView {
            content: "This dungeon no longer exists.".to_string(),
            components: vec![],
        });
    };

    let reactions = db::dungeon::get_reactions(pool, template.id).await?;
    let emoji_map = db::emoji::get_all_as_map(pool).await?;
    let category_emojis = db::emoji::list_by_categories(
        pool,
        &REACTION_CATEGORIES.iter().map(|(c, _)| *c).collect::<Vec<_>>(),
    )
    .await?;
    let by_category = group_by_category(&category_emojis);

    let guild_id = template
        .guild_id
        .expect("edit ephemeral only opens on guild-specific templates");
    let guild_tiers = db::tier::list(pool, guild_id).await?;

    let mut components: Vec<CreateActionRow> = Vec::with_capacity(5);

    // Reaction multi-selects, one per category. >25 entries per category
    // would need pagination — none currently approach that, so a future
    // optimisation. The select dispatches on submit via `de:react:...`.
    for (cat_logical, cat_label) in REACTION_CATEGORIES {
        let emojis = by_category.get(*cat_logical).cloned().unwrap_or_default();
        let selected: HashSet<&str> = reactions
            .iter()
            .map(|r| r.name.as_str())
            .filter(|n| emojis.iter().any(|e| e.logical_name == *n))
            .collect();
        let options: Vec<CreateSelectMenuOption> = emojis
            .iter()
            .take(25)
            .map(|e| {
                let label = snake_to_title(&e.logical_name);
                let mut opt = CreateSelectMenuOption::new(label, e.logical_name.clone());
                if let Some(rt) = emoji_rt(&e.logical_name, &emoji_map) {
                    opt = opt.emoji(rt);
                }
                if selected.contains(e.logical_name.as_str()) {
                    opt = opt.default_selection(true);
                }
                opt
            })
            .collect();

        if options.is_empty() {
            // Skip categories with no emojis uploaded yet — sync-wiki
            // hasn't been run, or the bot's emoji budget didn't include
            // this category.
            continue;
        }

        let max = options.len() as u8;
        let menu = CreateSelectMenu::new(
            format!("de:react:{}:{cat_logical}", template.id),
            CreateSelectMenuKind::String { options },
        )
        .placeholder(format!("{cat_label} (none required)"))
        .min_values(0)
        .max_values(max);
        components.push(CreateActionRow::SelectMenu(menu));
    }

    // Tier multi-select. Pre-compute the visible-tier set up front (one
    // query per tier — O(tiers) so cheap; avoids interleaving async work
    // inside the option-building closure).
    let mut visible_tier_ids: HashSet<i32> = HashSet::new();
    for t in &guild_tiers {
        if db::tier::is_dungeon_visible(pool, t.id, template.id, guild_id).await? {
            visible_tier_ids.insert(t.id);
        }
    }
    if !guild_tiers.is_empty() {
        let tier_options: Vec<CreateSelectMenuOption> = guild_tiers
            .iter()
            .take(25)
            .map(|t| {
                let mut opt = CreateSelectMenuOption::new(t.name.clone(), t.id.to_string());
                if visible_tier_ids.contains(&t.id) {
                    opt = opt.default_selection(true);
                }
                opt
            })
            .collect();
        let max = tier_options.len() as u8;
        let menu = CreateSelectMenu::new(
            format!("de:tiers:{}", template.id),
            CreateSelectMenuKind::String {
                options: tier_options,
            },
        )
        .placeholder("Tiers this dungeon is enabled in")
        .min_values(0)
        .max_values(max);
        components.push(CreateActionRow::SelectMenu(menu));
    }

    // Button row.
    let scalars_btn = CreateButton::new(format!("de:scalars:{}", template.id))
        .label("Edit text fields")
        .style(ButtonStyle::Secondary);
    let vc_label = if template.requires_vc {
        "VC required ✓"
    } else {
        "VC required ✗"
    };
    let vc_btn = CreateButton::new(format!("de:vc:{}", template.id))
        .label(vc_label)
        .style(ButtonStyle::Secondary);
    let tune_btn = CreateButton::new(format!("de:tune:{}", template.id))
        .label("Tune reactions")
        .style(ButtonStyle::Secondary)
        .disabled(reactions.is_empty());
    let done_btn = CreateButton::new("de:done:_".to_string())
        .label("Done")
        .style(ButtonStyle::Success);
    components.push(CreateActionRow::Buttons(vec![
        scalars_btn,
        vc_btn,
        tune_btn,
        done_btn,
    ]));

    let mut content = String::new();
    if let Some(note) = intro_note {
        content.push_str(note);
        content.push_str("\n\n");
    }
    content.push_str(&format_edit_summary(&template, &reactions));

    Ok(EditView {
        content,
        components,
    })
}

fn group_by_category(emojis: &[BotEmoji]) -> HashMap<String, Vec<BotEmoji>> {
    let mut out: HashMap<String, Vec<BotEmoji>> = HashMap::new();
    for e in emojis {
        if let Some(cat) = e.category.clone() {
            out.entry(cat).or_default().push(e.clone());
        }
    }
    out
}

fn format_edit_summary(template: &DungeonTemplate, reactions: &[DungeonReaction]) -> String {
    let color_str = template
        .color
        .map(|c| format!("`#{:06X}`", c))
        .unwrap_or_else(|| "_default_".to_string());
    let desc = template
        .message_description
        .as_deref()
        .map(|s| format!("\"{s}\""))
        .unwrap_or_else(|| "_unset_".to_string());
    format!(
        "**Editing `{slug}`** — display name **{name}**, color {color}, description {desc}, \
         {n} reaction(s) configured.\n\
         _Each change saves immediately. Click Done to close._",
        slug = template.name,
        name = template.display_name,
        color = color_str,
        desc = desc,
        n = reactions.len(),
    )
}

// ---------------------------------------------------------------------------
// Component handlers
// ---------------------------------------------------------------------------

async fn handle_reactions_submit(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    template_id: i32,
    category: &str,
) -> Result<(), BotError> {
    let ComponentInteractionDataKind::StringSelect { values } = &mci.data.kind else {
        return Ok(());
    };

    // Defence in depth: the edit ephemeral is only ever rendered with a
    // guild-specific template_id (create INSERTs one, edit forks globals
    // before opening). A stale custom_id from a pre-fork session pointing
    // at a global would mutate the global's reactions — refuse instead.
    let Some(template) = db::dungeon::get_by_id(&data.db, template_id).await? else {
        return Ok(());
    };
    if template.guild_id.is_none() {
        mci.create_response(
            ctx,
            ephemeral("This ephemeral is stale — re-run `/dungeon edit` to fork the global."),
        )
        .await?;
        return Ok(());
    }

    let new_set: HashSet<&str> = values.iter().map(|s| s.as_str()).collect();

    let current = db::dungeon::get_reactions(&data.db, template_id).await?;
    let category_emojis = db::emoji::list_by_categories(&data.db, &[category]).await?;
    let category_names: HashSet<&str> = category_emojis
        .iter()
        .map(|e| e.logical_name.as_str())
        .collect();

    // Compute totals BEFORE applying so we can reject early if the new
    // selection would push past the per-template cap.
    let outside_category_count = current
        .iter()
        .filter(|r| !category_names.contains(r.name.as_str()))
        .count();
    let prospective_total = outside_category_count + new_set.len();
    if prospective_total > limits::REACTIONS_PER_TEMPLATE {
        mci.create_response(
            ctx,
            ephemeral(format!(
                "Too many reactions — `{name}` already has {existing} configured, \
                 and Discord caps each message at {cap}.",
                name = category,
                existing = current.len(),
                cap = limits::REACTIONS_PER_TEMPLATE,
            )),
        )
        .await?;
        return Ok(());
    }

    // Removed: in this category, currently a reaction, but not in new_set.
    for r in &current {
        if category_names.contains(r.name.as_str()) && !new_set.contains(r.name.as_str()) {
            db::dungeon::delete_reaction_by_name(&data.db, template_id, &r.name).await?;
        }
    }

    // Added: in new_set but not currently a reaction.
    let current_names: HashSet<&str> = current.iter().map(|r| r.name.as_str()).collect();
    let next_sort_order = current
        .iter()
        .map(|r| r.sort_order)
        .max()
        .unwrap_or(0)
        + 1;
    let mut sort_offset = 0;
    for value in values {
        if current_names.contains(value.as_str()) {
            continue;
        }
        // Defaults: snake_to_title for display_name; keys are non-gating
        // (matches the recent default-reactions rule), everything else
        // gates with num_required = 1.
        let display_name = snake_to_title(value);
        let num_required = if value.contains("key") { 0 } else { 1 };
        let sort_order = next_sort_order + sort_offset;
        sort_offset += 1;
        db::dungeon::upsert_reaction(
            &data.db,
            template_id,
            value,
            &display_name,
            value,
            num_required,
            false,
            sort_order,
        )
        .await?;
    }

    update_with_view(ctx, mci, &data.db, template_id).await
}

async fn handle_tiers_submit(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    template_id: i32,
) -> Result<(), BotError> {
    let ComponentInteractionDataKind::StringSelect { values } = &mci.data.kind else {
        return Ok(());
    };
    let Some(template) = db::dungeon::get_by_id(&data.db, template_id).await? else {
        return Ok(());
    };
    let Some(guild_id) = template.guild_id else {
        return Ok(());
    };

    let selected: HashSet<i32> = values.iter().filter_map(|s| s.parse().ok()).collect();
    let guild_tiers = db::tier::list(&data.db, guild_id).await?;

    for tier in &guild_tiers {
        let visible =
            db::tier::is_dungeon_visible(&data.db, tier.id, template.id, guild_id).await?;
        let want_visible = selected.contains(&tier.id);
        match (visible, want_visible) {
            (true, false) => {
                db::tier::remove_dungeon(&data.db, tier.id, &template).await?;
            }
            (false, true) => {
                db::tier::add_dungeon(&data.db, tier.id, &template).await?;
            }
            _ => {}
        }
    }

    update_with_view(ctx, mci, &data.db, template_id).await
}

async fn handle_scalars_button(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    template_id: i32,
) -> Result<(), BotError> {
    let Some(template) = db::dungeon::get_by_id(&data.db, template_id).await? else {
        return Ok(());
    };
    let preset_color = template
        .color
        .map(|c| format!("{c:06X}"))
        .unwrap_or_default();
    let modal = CreateModal::new(format!("de:scalarsubmit:{template_id}"), "Edit text fields")
        .components(vec![
            CreateActionRow::InputText(
                CreateInputText::new(InputTextStyle::Short, "Display name", "display_name")
                    .value(template.display_name.clone())
                    .required(true)
                    .max_length(limits::DISPLAY_NAME_MAX as u16),
            ),
            CreateActionRow::InputText(
                CreateInputText::new(InputTextStyle::Paragraph, "Description", "description")
                    .value(template.message_description.clone().unwrap_or_default())
                    .required(false)
                    .max_length(limits::DESCRIPTION_MAX as u16),
            ),
            CreateActionRow::InputText(
                CreateInputText::new(InputTextStyle::Short, "Embed color hex", "color")
                    .value(preset_color)
                    .required(false)
                    .max_length(7),
            ),
        ]);
    mci.create_response(ctx, CreateInteractionResponse::Modal(modal))
        .await?;
    Ok(())
}

async fn handle_vc_toggle(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    template_id: i32,
) -> Result<(), BotError> {
    let Some(template) = db::dungeon::get_by_id(&data.db, template_id).await? else {
        return Ok(());
    };
    let Some(guild_id) = template.guild_id else {
        return Ok(());
    };
    let new_value = !template.requires_vc;
    db::dungeon::update_guild_template(
        &data.db,
        guild_id,
        &template.name,
        None,
        None,
        None,
        None,
        None,
        Some(new_value),
    )
    .await?;
    update_with_view(ctx, mci, &data.db, template_id).await
}

async fn handle_tune_open(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    template_id: i32,
) -> Result<(), BotError> {
    let reactions = db::dungeon::get_reactions(&data.db, template_id).await?;
    let emoji_map = db::emoji::get_all_as_map(&data.db).await?;

    if reactions.is_empty() {
        mci.create_response(
            ctx,
            ephemeral("No reactions to tune yet — pick some from the multi-selects first."),
        )
        .await?;
        return Ok(());
    }

    // One button per reaction. Discord caps Buttons rows at 5 buttons; we
    // chunk into multiple rows. Plus a final back-button row.
    let mut rows: Vec<CreateActionRow> = Vec::new();
    let mut buttons: Vec<CreateButton> = Vec::with_capacity(5);
    for r in &reactions {
        let mut btn = CreateButton::new(format!("de:tunepick:{}", r.id))
            .label(truncate_for_button(&r.display_name))
            .style(ButtonStyle::Secondary);
        if let Some(rt) = emoji_rt(&r.emoji, &emoji_map) {
            btn = btn.emoji(rt);
        }
        buttons.push(btn);
        if buttons.len() == 5 {
            rows.push(CreateActionRow::Buttons(std::mem::take(&mut buttons)));
            if rows.len() == 4 {
                // Reserve the last row for the Back button. Up to 20
                // reactions fit; cap (`limits::REACTIONS_PER_TEMPLATE`)
                // also enforces 20.
                break;
            }
        }
    }
    if !buttons.is_empty() && rows.len() < 4 {
        rows.push(CreateActionRow::Buttons(buttons));
    }
    rows.push(CreateActionRow::Buttons(vec![CreateButton::new(format!(
        "de:back:{template_id}"
    ))
    .label("← Back")
    .style(ButtonStyle::Primary)]));

    let resp = CreateInteractionResponse::UpdateMessage(
        CreateInteractionResponseMessage::new()
            .content(
                "**Tune a reaction** — click one to edit display name, num_required, sort \
                 order, and confirmation.",
            )
            .components(rows),
    );
    mci.create_response(ctx, resp).await?;
    Ok(())
}

async fn handle_tune_pick(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    reaction_id: i32,
) -> Result<(), BotError> {
    let Some(reaction) = db::dungeon::get_reaction(&data.db, reaction_id).await? else {
        mci.create_response(ctx, ephemeral("That reaction no longer exists."))
            .await?;
        return Ok(());
    };
    let modal = CreateModal::new(
        format!("de:tunesubmit:{reaction_id}"),
        format!("Tune reaction: {}", truncate_for_button(&reaction.display_name)),
    )
    .components(vec![
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Short, "Display name", "display_name")
                .value(reaction.display_name.clone())
                .required(true)
                .max_length(limits::REACTION_DISPLAY_NAME_MAX as u16),
        ),
        CreateActionRow::InputText(
            CreateInputText::new(
                InputTextStyle::Short,
                "Num required (0 = non-gating)",
                "num_required",
            )
            .value(reaction.num_required.to_string())
            .required(true)
            .max_length(3),
        ),
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Short, "Sort order", "sort_order")
                .value(reaction.sort_order.to_string())
                .required(true)
                .max_length(4),
        ),
        CreateActionRow::InputText(
            CreateInputText::new(
                InputTextStyle::Short,
                "Requires confirmation? (y/n)",
                "requires_confirmation",
            )
            .value(if reaction.requires_confirmation { "y" } else { "n" }.to_string())
            .required(true)
            .max_length(3),
        ),
    ]);
    mci.create_response(ctx, CreateInteractionResponse::Modal(modal))
        .await?;
    Ok(())
}

async fn handle_back(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    template_id: i32,
) -> Result<(), BotError> {
    update_with_view(ctx, mci, &data.db, template_id).await
}

async fn handle_done(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
) -> Result<(), BotError> {
    let resp = CreateInteractionResponse::UpdateMessage(
        CreateInteractionResponseMessage::new()
            .content("Saved. Run `/dungeon edit` to come back any time.")
            .components(vec![]),
    );
    mci.create_response(ctx, resp).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Modal handlers
// ---------------------------------------------------------------------------

async fn handle_create_submit(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    data: &BotData,
    inherit: Option<i32>,
) -> Result<(), BotError> {
    let Some(guild_id) = modal.guild_id.map(|g| g.get() as i64) else {
        modal
            .create_response(ctx, ephemeral("This command only works in a server."))
            .await?;
        return Ok(());
    };

    let display_name = extract_input(modal, "display_name").unwrap_or_default();
    let display_name = display_name.trim();
    if display_name.is_empty() {
        modal
            .create_response(ctx, ephemeral("Display name can't be empty."))
            .await?;
        return Ok(());
    }
    if display_name.chars().count() > limits::DISPLAY_NAME_MAX {
        modal
            .create_response(
                ctx,
                ephemeral(format!(
                    "Display name is too long (max {}).",
                    limits::DISPLAY_NAME_MAX
                )),
            )
            .await?;
        return Ok(());
    }

    let slug = slug_from_display(display_name);
    if slug.is_empty() {
        modal
            .create_response(
                ctx,
                ephemeral(
                    "Display name needs at least one letter or digit so we can derive a slug.",
                ),
            )
            .await?;
        return Ok(());
    }
    if slug.len() > limits::TEMPLATE_NAME_MAX {
        modal
            .create_response(
                ctx,
                ephemeral(format!(
                    "Derived slug `{slug}` is too long (max {} chars). Pick a shorter name.",
                    limits::TEMPLATE_NAME_MAX
                )),
            )
            .await?;
        return Ok(());
    }

    if db::dungeon::get_by_name(&data.db, guild_id, &slug)
        .await?
        .is_some()
    {
        modal
            .create_response(
                ctx,
                ephemeral(format!(
                    "Slug `{slug}` already exists for this server (or as a global). \
                     Pick a different display name."
                )),
            )
            .await?;
        return Ok(());
    }

    let description_raw = extract_input(modal, "description").unwrap_or_default();
    let description = description_raw.trim();
    let description: Option<&str> = if description.is_empty() {
        None
    } else {
        Some(description)
    };

    let color_raw = extract_input(modal, "color").unwrap_or_default();
    let color = match parse_color_hex(color_raw.trim()) {
        Ok(c) => c,
        Err(msg) => {
            modal.create_response(ctx, ephemeral(msg)).await?;
            return Ok(());
        }
    };

    // Cap check just before INSERT — race-resistant enough for human-paced
    // /dungeon create (the modal already gated us on "open this form").
    let count = db::dungeon::count_guild_templates(&data.db, guild_id).await?;
    if count >= limits::CUSTOM_DUNGEONS_PER_GUILD {
        modal
            .create_response(
                ctx,
                ephemeral(format!(
                    "This server is at the cap of {} custom dungeons. \
                     Delete one with `/dungeon delete` before creating another.",
                    limits::CUSTOM_DUNGEONS_PER_GUILD
                )),
            )
            .await?;
        return Ok(());
    }

    let new_id = db::dungeon::create_guild_template_with_inherit(
        &data.db,
        db::dungeon::CreateGuildTemplateParams {
            guild_id,
            name: &slug,
            display_name,
            description,
            color,
            inherit_from: inherit,
        },
    )
    .await?;

    let intro = format!(
        "Created `{slug}`. Tune reactions, tiers, and text fields below."
    );
    let resp = build_edit_response(&data.db, new_id, Some(&intro)).await?;
    modal
        .create_response(ctx, CreateInteractionResponse::Message(resp))
        .await?;
    Ok(())
}

async fn handle_scalars_submit(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    data: &BotData,
    template_id: i32,
) -> Result<(), BotError> {
    let Some(template) = db::dungeon::get_by_id(&data.db, template_id).await? else {
        modal
            .create_response(ctx, ephemeral("This dungeon no longer exists."))
            .await?;
        return Ok(());
    };
    let Some(guild_id) = template.guild_id else {
        modal
            .create_response(ctx, ephemeral("Globals can't be edited directly."))
            .await?;
        return Ok(());
    };

    let display_name = extract_input(modal, "display_name").unwrap_or_default();
    let display_name = display_name.trim();
    if display_name.is_empty() {
        modal
            .create_response(ctx, ephemeral("Display name can't be empty."))
            .await?;
        return Ok(());
    }
    if display_name.chars().count() > limits::DISPLAY_NAME_MAX {
        modal
            .create_response(
                ctx,
                ephemeral(format!(
                    "Display name is too long (max {}).",
                    limits::DISPLAY_NAME_MAX
                )),
            )
            .await?;
        return Ok(());
    }

    let description_raw = extract_input(modal, "description").unwrap_or_default();
    let description = description_raw.trim();
    let description_for_update: Option<&str> = if description.is_empty() {
        // Caller wants to clear it. update_guild_template uses COALESCE so
        // None preserves; we need a sentinel-y approach. For this v1 we
        // pass an explicit empty string when the user blanked the field.
        Some("")
    } else {
        Some(description)
    };

    let color_raw = extract_input(modal, "color").unwrap_or_default();
    let color = match parse_color_hex(color_raw.trim()) {
        Ok(c) => c,
        Err(msg) => {
            modal.create_response(ctx, ephemeral(msg)).await?;
            return Ok(());
        }
    };

    db::dungeon::update_guild_template(
        &data.db,
        guild_id,
        &template.name,
        Some(display_name),
        None,
        color,
        None,
        description_for_update,
        None,
    )
    .await?;

    let view = render_edit_view(&data.db, template_id, None).await?;
    modal
        .create_response(
            ctx,
            CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new()
                    .content(view.content)
                    .components(view.components),
            ),
        )
        .await?;
    Ok(())
}

async fn handle_tune_submit(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    data: &BotData,
    reaction_id: i32,
) -> Result<(), BotError> {
    let Some(reaction) = db::dungeon::get_reaction(&data.db, reaction_id).await? else {
        modal
            .create_response(ctx, ephemeral("That reaction no longer exists."))
            .await?;
        return Ok(());
    };

    // Defence in depth: refuse to mutate reactions that belong to a
    // global template. The edit ephemeral forks globals before exposing
    // any tuning UI, so a stale reaction_id pointing at a global means
    // the user is on an out-of-date ephemeral.
    let parent = db::dungeon::get_by_id(&data.db, reaction.dungeon_template_id).await?;
    if parent.as_ref().and_then(|p| p.guild_id).is_none() {
        modal
            .create_response(
                ctx,
                ephemeral("This ephemeral is stale — re-run `/dungeon edit` to fork the global."),
            )
            .await?;
        return Ok(());
    }

    let display_name = extract_input(modal, "display_name").unwrap_or_default();
    let display_name = display_name.trim();
    if display_name.is_empty() || display_name.chars().count() > limits::REACTION_DISPLAY_NAME_MAX {
        modal
            .create_response(
                ctx,
                ephemeral(format!(
                    "Display name must be 1-{} chars.",
                    limits::REACTION_DISPLAY_NAME_MAX
                )),
            )
            .await?;
        return Ok(());
    }

    let num_required: i32 = match extract_input(modal, "num_required")
        .unwrap_or_default()
        .trim()
        .parse()
    {
        Ok(n) if (0..=20).contains(&n) => n,
        _ => {
            modal
                .create_response(ctx, ephemeral("`num_required` must be a number 0-20."))
                .await?;
            return Ok(());
        }
    };

    let sort_order: i32 = match extract_input(modal, "sort_order")
        .unwrap_or_default()
        .trim()
        .parse()
    {
        Ok(n) => n,
        Err(_) => {
            modal
                .create_response(ctx, ephemeral("`sort_order` must be an integer."))
                .await?;
            return Ok(());
        }
    };

    let confirm_raw = extract_input(modal, "requires_confirmation").unwrap_or_default();
    let requires_confirmation = match confirm_raw.trim().to_lowercase().as_str() {
        "y" | "yes" | "true" | "1" => true,
        "n" | "no" | "false" | "0" => false,
        _ => {
            modal
                .create_response(
                    ctx,
                    ephemeral("`requires_confirmation` must be `y`/`n`."),
                )
                .await?;
            return Ok(());
        }
    };

    db::dungeon::update_reaction(
        &data.db,
        reaction_id,
        display_name,
        num_required,
        sort_order,
        requires_confirmation,
    )
    .await?;

    let view = render_edit_view(&data.db, reaction.dungeon_template_id, None).await?;
    modal
        .create_response(
            ctx,
            CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new()
                    .content(view.content)
                    .components(view.components),
            ),
        )
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ephemeral(text: impl Into<String>) -> CreateInteractionResponse {
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

/// Parse "FF4500" / "#FF4500" / "" / whitespace into Some/None color int.
/// Returns Err with a user-facing message on malformed input.
fn parse_color_hex(s: &str) -> Result<Option<i32>, String> {
    if s.is_empty() {
        return Ok(None);
    }
    let hex = s.trim_start_matches('#');
    match i64::from_str_radix(hex, 16) {
        Ok(v) if v <= 0xFFFFFF => Ok(Some(v as i32)),
        _ => Err("Color must be a hex triplet like `FF4500`.".to_string()),
    }
}

async fn update_with_view(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    pool: &PgPool,
    template_id: i32,
) -> Result<(), BotError> {
    let view = render_edit_view(pool, template_id, None).await?;
    let resp = CreateInteractionResponse::UpdateMessage(
        CreateInteractionResponseMessage::new()
            .content(view.content)
            .components(view.components),
    );
    mci.create_response(ctx, resp).await?;
    Ok(())
}

fn truncate_for_button(s: &str) -> String {
    // Discord button labels max out at 80 chars; keep some margin.
    if s.chars().count() <= 64 {
        s.to_string()
    } else {
        s.chars().take(63).collect::<String>() + "…"
    }
}

