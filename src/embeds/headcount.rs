use std::collections::HashMap;

use poise::serenity_prelude as serenity;
use serenity::{ButtonStyle, CreateActionRow, CreateButton, CreateEmbed, EmojiId, ReactionType};

use crate::db::headcount::ReactionCount;
use crate::db::models::{BagTier, BotEmoji, DungeonReaction, DungeonTemplate};
use crate::embeds::build_loot_fields;

/// A reaction's `emoji` field is normally a logical name that maps to a
/// custom `bot_emoji` row, but some built-ins (notably the "Reacts" interest
/// reaction) use a literal Unicode emoji so they render before `sync-wiki`
/// has uploaded the custom set. Logical names are ASCII [a-z0-9_]+; anything
/// with a non-ASCII char is treated as a Unicode literal.
fn is_unicode_literal(s: &str) -> bool {
    !s.is_empty() && s.chars().any(|c| !c.is_ascii())
}

/// Returns a `<:name:id>` / `<a:name:id>` string for use in embed text fields.
pub fn emoji_str(logical_name: &str, map: &HashMap<String, BotEmoji>) -> String {
    match map.get(logical_name) {
        Some(e) if e.animated => format!("<a:{}:{}>", e.name_on_discord, e.discord_emoji_id),
        Some(e) => format!("<:{}:{}>", e.name_on_discord, e.discord_emoji_id),
        None if is_unicode_literal(logical_name) => logical_name.to_string(),
        None => String::new(),
    }
}

/// Returns a `ReactionType` for use in button `.emoji()`.
pub fn emoji_rt(logical_name: &str, map: &HashMap<String, BotEmoji>) -> Option<ReactionType> {
    if let Some(e) = map.get(logical_name) {
        Some(ReactionType::Custom {
            animated: e.animated,
            id: EmojiId::new(e.discord_emoji_id as u64),
            name: Some(e.name_on_discord.clone()),
        })
    } else if is_unicode_literal(logical_name) {
        Some(ReactionType::Unicode(logical_name.to_string()))
    } else {
        None
    }
}

/// Build the headcount embed and action rows.
#[allow(clippy::too_many_arguments)]
pub fn build(
    headcount_id: i32,
    template: &DungeonTemplate,
    reactions: &[DungeonReaction],
    counts: &HashMap<i32, ReactionCount>,
    emoji_map: &HashMap<String, BotEmoji>,
    leader_id: u64,
    bag_tiers: &[BagTier],
    threshold: &str,
) -> (CreateEmbed, Vec<CreateActionRow>) {
    let color = template.color.unwrap_or(0x5865F2) as u32;

    let default_title = format!("{} Headcount", template.display_name);
    let title = template.message_title.as_deref().unwrap_or(&default_title);

    let base_desc = template
        .message_description
        .as_deref()
        .unwrap_or("React below to sign up! Click a button again to withdraw.");

    let description = format!("{base_desc}\n\nLeader: <@{leader_id}>");

    let mut fields: Vec<(String, String, bool)> = reactions
        .iter()
        .map(|r| {
            let cnt = counts.get(&r.id);
            let total = cnt.map(|c| c.total).unwrap_or(0);
            let confirmed = cnt.map(|c| c.confirmed).unwrap_or(0);
            let es = emoji_str(&r.emoji, emoji_map);

            let field_name = if es.is_empty() {
                r.display_name.clone()
            } else {
                format!("{es} {}", r.display_name)
            };

            let field_val = if r.requires_confirmation {
                format!("{confirmed}/{} confirmed ✅", r.num_required)
            } else {
                format!("{total} interested")
            };

            (field_name, field_val, true)
        })
        .collect();

    fields.extend(build_loot_fields(
        &template.showcase_emoji,
        emoji_map,
        bag_tiers,
        threshold,
    ));

    let mut embed = CreateEmbed::default()
        .title(title)
        .description(&description)
        .color(color)
        .fields(fields);

    if let Some(url) = &template.thumbnail_url {
        embed = embed.thumbnail(url);
    }

    // Reaction buttons — one per dungeon reaction, max 5 per row.
    let reaction_buttons: Vec<CreateButton> = reactions
        .iter()
        .map(|r| {
            let mut btn = CreateButton::new(format!("hc:{headcount_id}:react:{}", r.id))
                .label(&r.display_name)
                .style(ButtonStyle::Secondary);
            if let Some(rt) = emoji_rt(&r.emoji, emoji_map) {
                btn = btn.emoji(rt);
            }
            btn
        })
        .collect();

    let start_btn = CreateButton::new(format!("hc:{headcount_id}:start"))
        .label("Start Run")
        .emoji(ReactionType::Unicode("🚀".to_string()))
        .style(ButtonStyle::Success);
    let cancel_btn = CreateButton::new(format!("hc:{headcount_id}:cancel"))
        .label("Cancel")
        .emoji(ReactionType::Unicode("🗑️".to_string()))
        .style(ButtonStyle::Danger);

    let mut rows: Vec<CreateActionRow> = reaction_buttons
        .chunks(5)
        .map(|chunk| CreateActionRow::Buttons(chunk.to_vec()))
        .collect();
    rows.push(CreateActionRow::Buttons(vec![start_btn, cancel_btn]));

    (embed, rows)
}

/// Build the closed/ended version of the headcount embed (no buttons).
pub fn build_closed(
    template: &DungeonTemplate,
    emoji_map: &HashMap<String, BotEmoji>,
    reason: &str,
    cancelled: bool,
) -> CreateEmbed {
    let color: u32 = if cancelled { 0x808080 } else { 0x57F287 };

    let default_title = format!("{} Headcount", template.display_name);
    let title = template.message_title.as_deref().unwrap_or(&default_title);

    let es = template
        .emoji
        .as_deref()
        .map(|e| emoji_str(e, emoji_map))
        .unwrap_or_default();

    let full_title = if es.is_empty() {
        title.to_string()
    } else {
        format!("{es} {title}")
    };

    CreateEmbed::default()
        .title(full_title)
        .description(reason)
        .color(color)
}
