use std::collections::HashMap;

use poise::serenity_prelude as serenity;
use serenity::{ButtonStyle, CreateActionRow, CreateButton, CreateEmbed, ReactionType};

use crate::db::models::{BagTier, BotEmoji, DungeonReaction, DungeonTemplate, Run};
use crate::db::run::Participant;
use crate::embeds::build_loot_fields;
use crate::embeds::headcount::{emoji_rt, emoji_str};

/// Build the active run embed + its public action rows (Join / Leave /
/// Control Panel, plus one Confirm button per `requires_confirmation`
/// reaction).
#[allow(clippy::too_many_arguments)]
pub fn build(
    run: &Run,
    template: &DungeonTemplate,
    reactions: &[DungeonReaction],
    participants: &[Participant],
    emoji_map: &HashMap<String, BotEmoji>,
    bag_tiers: &[BagTier],
    threshold: &str,
) -> (CreateEmbed, Vec<CreateActionRow>) {
    let color = template.color.unwrap_or(0x5865F2) as u32;

    let default_title = format!("{} Run", template.display_name);
    let title = template.message_title.as_deref().unwrap_or(&default_title);

    let template_emoji = template
        .emoji
        .as_deref()
        .map(|e| emoji_str(e, emoji_map))
        .unwrap_or_default();
    let full_title = if template_emoji.is_empty() {
        title.to_string()
    } else {
        format!("{template_emoji} {title}")
    };

    let mut description = format!("**Leader:** <@{}>", run.leader_user_id);
    if let Some(loc) = &run.location {
        description.push_str(&format!("\n**Location:** `{loc}`"));
    }
    if let Some(party) = &run.party {
        description.push_str(&format!("\n**Party:** {party}"));
    }
    if let Some(vc_id) = run.voice_channel_id {
        description.push_str(&format!("\n**Voice:** <#{vc_id}>"));
    }

    // Per-reaction participant breakdown. Each field lists users who
    // declared that item, checkmarking confirmed ones. Reactions with
    // zero participants still render so the list of items is visible.
    let mut fields: Vec<(String, String, bool)> = Vec::with_capacity(reactions.len() + 1);
    for r in reactions {
        let users: Vec<_> = participants
            .iter()
            .filter(|p| p.dungeon_reaction_id == Some(r.id))
            .collect();

        let value = if users.is_empty() {
            "_none_".to_string()
        } else {
            users
                .iter()
                .map(|p| {
                    if p.confirmed {
                        format!("✅ <@{}>", p.user_id)
                    } else {
                        format!("<@{}>", p.user_id)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        let es = emoji_str(&r.emoji, emoji_map);
        let name = if es.is_empty() {
            format!("{} ({}/{})", r.display_name, users.len(), r.num_required)
        } else {
            format!("{es} {} ({}/{})", r.display_name, users.len(), r.num_required)
        };
        fields.push((name, value, true));
    }

    // "Joined" roster: distinct users across the run, regardless of item.
    let mut joined: Vec<i64> = participants.iter().map(|p| p.user_id).collect();
    joined.sort();
    joined.dedup();
    let joined_value = if joined.is_empty() {
        "_none_".to_string()
    } else {
        joined
            .iter()
            .map(|u| format!("<@{u}>"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    fields.push((
        format!("👥 Joined ({})", joined.len()),
        joined_value,
        false,
    ));

    fields.extend(build_loot_fields(
        &template.showcase_emoji,
        emoji_map,
        bag_tiers,
        threshold,
    ));

    let mut embed = CreateEmbed::default()
        .title(full_title)
        .description(&description)
        .color(color)
        .fields(fields);

    if let Some(url) = &template.thumbnail_url {
        embed = embed.thumbnail(url);
    }

    // Public components: row 1 is Join / Leave / Control Panel; additional
    // rows (up to 5 buttons each) are per-confirmation-reaction Confirm
    // buttons so anyone bringing a key/rune can declare it without digging
    // into a menu.
    let join_btn = CreateButton::new(format!("run:{}:join", run.id))
        .label("Join")
        .emoji(ReactionType::Unicode("✋".into()))
        .style(ButtonStyle::Success);
    let leave_btn = CreateButton::new(format!("run:{}:leave", run.id))
        .label("Leave")
        .emoji(ReactionType::Unicode("🚪".into()))
        .style(ButtonStyle::Secondary);
    let cp_btn = CreateButton::new(format!("run:{}:cp", run.id))
        .label("Control Panel")
        .emoji(ReactionType::Unicode("⚙️".into()))
        .style(ButtonStyle::Primary);

    let mut rows = vec![CreateActionRow::Buttons(vec![join_btn, leave_btn, cp_btn])];

    let confirm_buttons: Vec<CreateButton> = reactions
        .iter()
        .filter(|r| r.requires_confirmation)
        .map(|r| {
            let mut btn = CreateButton::new(format!("run:{}:confirm:{}", run.id, r.id))
                .label(format!("Bring {}", r.display_name))
                .style(ButtonStyle::Secondary);
            if let Some(rt) = emoji_rt(&r.emoji, emoji_map) {
                btn = btn.emoji(rt);
            }
            btn
        })
        .collect();

    for chunk in confirm_buttons.chunks(5) {
        rows.push(CreateActionRow::Buttons(chunk.to_vec()));
    }

    (embed, rows)
}

/// Ended-run embed (no components). Grey color, strikethrough-style header.
#[allow(clippy::too_many_arguments)]
pub fn build_ended(
    run: &Run,
    template: &DungeonTemplate,
    reactions: &[DungeonReaction],
    participants: &[Participant],
    emoji_map: &HashMap<String, BotEmoji>,
    bag_tiers: &[BagTier],
    threshold: &str,
) -> CreateEmbed {
    let default_title = format!("{} Run", template.display_name);
    let title = template.message_title.as_deref().unwrap_or(&default_title);

    let template_emoji = template
        .emoji
        .as_deref()
        .map(|e| emoji_str(e, emoji_map))
        .unwrap_or_default();
    let full_title = if template_emoji.is_empty() {
        format!("{title} — ended")
    } else {
        format!("{template_emoji} {title} — ended")
    };

    let mut description = format!("**Leader:** <@{}>", run.leader_user_id);
    if let Some(loc) = &run.location {
        description.push_str(&format!("\n**Location:** `{loc}`"));
    }

    let mut joined: Vec<i64> = participants.iter().map(|p| p.user_id).collect();
    joined.sort();
    joined.dedup();

    let mut fields: Vec<(String, String, bool)> = Vec::new();
    // Keep the per-item breakdown so post-run context ("who brought what")
    // is preserved on the ended embed.
    for r in reactions {
        let users: Vec<_> = participants
            .iter()
            .filter(|p| p.dungeon_reaction_id == Some(r.id))
            .collect();
        if users.is_empty() {
            continue;
        }
        let es = emoji_str(&r.emoji, emoji_map);
        let name = if es.is_empty() {
            r.display_name.clone()
        } else {
            format!("{es} {}", r.display_name)
        };
        let value = users
            .iter()
            .map(|p| {
                if p.confirmed {
                    format!("✅ <@{}>", p.user_id)
                } else {
                    format!("<@{}>", p.user_id)
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        fields.push((name, value, true));
    }
    fields.push((
        format!("👥 Joined ({})", joined.len()),
        if joined.is_empty() {
            "_none_".to_string()
        } else {
            joined.iter().map(|u| format!("<@{u}>")).collect::<Vec<_>>().join(", ")
        },
        false,
    ));

    fields.extend(build_loot_fields(
        &template.showcase_emoji,
        emoji_map,
        bag_tiers,
        threshold,
    ));

    CreateEmbed::default()
        .title(full_title)
        .description(&description)
        .color(0x808080u32)
        .fields(fields)
}

/// Leader-only control panel (ephemeral). Each button is a separate
/// interaction so the public embed doesn't need to reflow when the leader
/// manages things.
pub fn control_panel(run: &Run) -> (CreateEmbed, Vec<CreateActionRow>) {
    let embed = CreateEmbed::default()
        .title("Run Control Panel")
        .description(format!(
            "You are leader of run **#{}**. Actions here are private.\n\n\
             • **Set Location** — tell the party where to meet\n\
             • **Set Party** — free-form party composition note\n\
             • **Transfer Leader** — hand the raid to someone else\n\
             • **End Run** — close the run message and finish up",
            run.id
        ))
        .color(0x5865F2u32);

    let loc_btn = CreateButton::new(format!("run:{}:loc", run.id))
        .label("Set Location")
        .emoji(ReactionType::Unicode("📍".into()))
        .style(ButtonStyle::Primary);
    let party_btn = CreateButton::new(format!("run:{}:party", run.id))
        .label("Set Party")
        .emoji(ReactionType::Unicode("📝".into()))
        .style(ButtonStyle::Primary);
    let transfer_btn = CreateButton::new(format!("run:{}:transfer", run.id))
        .label("Transfer Leader")
        .emoji(ReactionType::Unicode("🔁".into()))
        .style(ButtonStyle::Secondary);
    let end_btn = CreateButton::new(format!("run:{}:end", run.id))
        .label("End Run")
        .emoji(ReactionType::Unicode("🛑".into()))
        .style(ButtonStyle::Danger);

    let rows = vec![
        CreateActionRow::Buttons(vec![loc_btn, party_btn]),
        CreateActionRow::Buttons(vec![transfer_btn, end_btn]),
    ];

    (embed, rows)
}
