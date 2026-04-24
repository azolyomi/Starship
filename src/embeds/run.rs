use std::collections::HashMap;

use poise::serenity_prelude as serenity;
use serenity::{ButtonStyle, CreateActionRow, CreateButton, CreateEmbed, ReactionType};

use crate::db::models::{BagTier, BotEmoji, DungeonReaction, DungeonTemplate, Run};
use crate::embeds::build_loot_fields;
use crate::embeds::headcount::emoji_str;

/// Build the active run embed + its public action row. R4: no Join / Leave
/// buttons, no per-reaction fields, no roster. Users react via native Discord
/// reactions attached to the message itself. The only public button is
/// Control Panel (leader / ManageRuns gated on click).
pub fn build(
    run: &Run,
    template: &DungeonTemplate,
    reactions: &[DungeonReaction],
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

    // Inline "what to bring" hint so users know which reactions to click.
    if !reactions.is_empty() {
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
        description.push_str(&format!("\n\n**React with:** {}", parts.join(" · ")));
    }

    let fields = build_loot_fields(
        &template.showcase_emoji,
        emoji_map,
        bag_tiers,
        threshold,
    );

    let mut embed = CreateEmbed::default()
        .title(full_title)
        .description(&description)
        .color(color)
        .fields(fields);

    if let Some(url) = &template.thumbnail_url {
        embed = embed.thumbnail(url);
    }

    let cp_btn = CreateButton::new(format!("run:{}:cp", run.id))
        .label("Control Panel")
        .emoji(ReactionType::Unicode("⚙️".into()))
        .style(ButtonStyle::Primary);

    let rows = vec![CreateActionRow::Buttons(vec![cp_btn])];

    (embed, rows)
}

/// Minimal greyed ended embed. R4: no roster, no item breakdown — just
/// "this run is over, here's who led it and where they went".
pub fn build_ended(
    run: &Run,
    template: &DungeonTemplate,
    emoji_map: &HashMap<String, BotEmoji>,
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

    CreateEmbed::default()
        .title(full_title)
        .description(&description)
        .color(0x808080u32)
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
