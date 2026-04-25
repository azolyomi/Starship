use std::collections::HashMap;

use poise::serenity_prelude as serenity;
use serenity::{ButtonStyle, CreateActionRow, CreateButton, CreateEmbed, EmojiId, ReactionType};

use crate::db::models::{BagTier, BotEmoji, DungeonReaction, DungeonTemplate};
use crate::embeds::build_loot_fields_all;

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

/// Returns a `ReactionType` suitable for both button `.emoji()` and
/// `Message::react` (native reactions). Falls through to `None` if the
/// logical name has no bot_emoji row yet and isn't a unicode literal —
/// callers should skip rather than invent a placeholder.
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
///
/// R4: per-item tracking is gone — users sign up by clicking Discord
/// reactions attached to the message itself. The embed just describes the
/// raid + lists what items are required, and the only buttons left are
/// organizer-level controls (Start Run / Cancel).
pub fn build(
    headcount_id: i32,
    template: &DungeonTemplate,
    reactions: &[DungeonReaction],
    emoji_map: &HashMap<String, BotEmoji>,
    leader_id: u64,
    bag_tiers: &[BagTier],
) -> (CreateEmbed, Vec<CreateActionRow>) {
    let color = template.color.unwrap_or(0x5865F2) as u32;

    let default_title = format!("{} Headcount", template.display_name);
    let title = template.message_title.as_deref().unwrap_or(&default_title);

    let base_desc = template
        .message_description
        .as_deref()
        .unwrap_or("React below to sign up!");

    // Required-items summary inline in the description so users see what
    // they need without extra fields. Each item shows its emoji + name.
    let required_line = if reactions.is_empty() {
        String::new()
    } else {
        let parts: Vec<String> = reactions
            .iter()
            .map(|r| {
                let es = emoji_str(&r.emoji, emoji_map);
                if es.is_empty() {
                    r.display_name.clone()
                } else {
                    format!("{es} {}", r.display_name)
                }
            })
            .collect();
        format!("\n\n**React with:** {}", parts.join(" · "))
    };

    let description = format!("{base_desc}\n\nLeader: <@{leader_id}>{required_line}");

    // Headcounts show every classified drop — the signup decision happens
    // here, so raiders want the full loot picture regardless of the guild's
    // run-view threshold.
    let fields = build_loot_fields_all(&template.showcase_emoji, emoji_map, bag_tiers);

    let mut embed = CreateEmbed::default()
        .title(title)
        .description(&description)
        .color(color)
        .fields(fields);

    if let Some(url) = &template.thumbnail_url {
        embed = embed.thumbnail(url);
    }

    let start_btn = CreateButton::new(format!("hc:{headcount_id}:start"))
        .label("Start Run")
        .emoji(ReactionType::Unicode("🚀".to_string()))
        .style(ButtonStyle::Success);
    let cancel_btn = CreateButton::new(format!("hc:{headcount_id}:cancel"))
        .label("Cancel")
        .emoji(ReactionType::Unicode("🗑️".to_string()))
        .style(ButtonStyle::Danger);

    let rows = vec![CreateActionRow::Buttons(vec![start_btn, cancel_btn])];

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
