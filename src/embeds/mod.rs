pub mod headcount;
pub mod run;

use std::collections::HashMap;

use crate::db::loot::resolve_bag_emoji;
use crate::db::models::{BagTier, BotEmoji};

/// Render a stored `BotEmoji` directly (no logical-name lookup).
pub fn render_bot_emoji(e: &BotEmoji) -> String {
    if e.animated {
        format!("<a:{}:{}>", e.name_on_discord, e.discord_emoji_id)
    } else {
        format!("<:{}:{}>", e.name_on_discord, e.discord_emoji_id)
    }
}

fn tier_display_name(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Build embed fields for a dungeon's loot drops, grouped by bag tier and
/// filtered to tiers whose sort_order is ≥ the configured threshold.
/// Rendered from highest tier (white) down to the threshold so the most
/// valuable drops sit at the top of the embed.
///
/// Drops without a `bag_tier` classification in `bot_emoji` (e.g. the
/// scraper hasn't reached the item page yet) are skipped — they'll render
/// again after the next `sync-wiki`.
pub fn build_loot_fields(
    showcase_emoji: &[String],
    emoji_map: &HashMap<String, BotEmoji>,
    bag_tiers: &[BagTier],
    threshold_tier_name: &str,
) -> Vec<(String, String, bool)> {
    // Unknown threshold name = show nothing. Real rows FK to bag_tiers, so
    // this only trips on bag_tiers being empty (pre-migration state).
    let threshold_order = match bag_tiers.iter().find(|t| t.name == threshold_tier_name) {
        Some(t) => t.sort_order,
        None => return Vec::new(),
    };

    let mut by_tier: HashMap<&str, Vec<&BotEmoji>> = HashMap::new();
    for logical in showcase_emoji {
        let Some(e) = emoji_map.get(logical) else { continue };
        let Some(tier) = e.bag_tier.as_deref() else { continue };
        by_tier.entry(tier).or_default().push(e);
    }

    let mut sorted: Vec<&BagTier> = bag_tiers.iter().collect();
    sorted.sort_by(|a, b| b.sort_order.cmp(&a.sort_order));

    let mut fields = Vec::new();
    for tier in sorted {
        if tier.sort_order < threshold_order {
            continue;
        }
        let Some(drops) = by_tier.get(tier.name.as_str()) else { continue };
        if drops.is_empty() {
            continue;
        }
        let rendered: Vec<String> = drops.iter().map(|e| render_bot_emoji(e)).collect();
        let bag_icon = resolve_bag_emoji(tier, emoji_map);
        let field_name = format!("{bag_icon} {} Bag", tier_display_name(&tier.name));
        fields.push((field_name, rendered.join(" "), false));
    }

    fields
}
