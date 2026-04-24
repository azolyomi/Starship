//! `/pingroles` — dungeon-notification-role management.
//!
//! Three flows:
//! * `/pingroles` (anyone) — paginated ephemeral opt-in picker that diffs
//!   the user's desired subscriptions against their actual role membership
//!   and applies add/remove with retries.
//! * `/pingroles set <dungeon> <role>` (admin) — bind an existing role.
//! * `/pingroles unset <dungeon>` (admin) — clear the binding.
//! * `/pingroles create <dungeon>` (admin) — create a fresh mentionable role
//!   named `"<display name> Pings"` and bind it.

use std::collections::HashSet;
use std::time::Duration;

use poise::serenity_prelude as serenity;
use poise::CreateReply;
use serenity::{
    ButtonStyle, ComponentInteractionCollector, ComponentInteractionDataKind, CreateActionRow,
    CreateButton, CreateEmbed, CreateInteractionResponse, CreateInteractionResponseMessage,
    CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption, EditRole, Permissions,
    RoleId,
};

use crate::db::models::DungeonTemplate;
use crate::{
    db,
    services::permission::{self as perm_svc, Action},
    BotContext, BotError,
};

const PAGE_SIZE: usize = 10;
const PICKER_TIMEOUT: Duration = Duration::from_secs(600);
const ROLE_MUTATION_ATTEMPTS: usize = 3;

async fn autocomplete_dungeon<'a>(
    ctx: BotContext<'_>,
    partial: &'a str,
) -> impl Iterator<Item = String> + 'a {
    let guild_id = match ctx.guild_id() {
        Some(id) => id.get() as i64,
        None => return Vec::new().into_iter(),
    };
    db::dungeon::list_for_guild(&ctx.data().db, guild_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(move |d| d.name.to_lowercase().contains(&partial.to_lowercase()))
        .map(|d| d.name)
        .collect::<Vec<_>>()
        .into_iter()
}

fn ephemeral(msg: impl Into<String>) -> CreateReply {
    CreateReply::default().content(msg).ephemeral(true)
}

/// Manage dungeon notification-role subscriptions.
#[poise::command(
    slash_command,
    guild_only,
    subcommands("set_", "unset", "create"),
    subcommand_required = false
)]
pub async fn pingroles(ctx: BotContext<'_>) -> Result<(), BotError> {
    // No subcommand → launch the self-service picker.
    self_service_picker(ctx).await
}

// ---------------------------------------------------------------------------
// Self-service picker
// ---------------------------------------------------------------------------

async fn self_service_picker(ctx: BotContext<'_>) -> Result<(), BotError> {
    let guild_id = ctx.guild_id().unwrap().get() as i64;
    let pool = &ctx.data().db;

    let dungeons: Vec<DungeonTemplate> = db::dungeon::list_for_guild(pool, guild_id)
        .await?
        .into_iter()
        .filter(|d| d.notification_role_id.is_some())
        .collect();

    if dungeons.is_empty() {
        ctx.send(ephemeral(
            "No dungeons have notification roles set up in this server yet. \
             An admin can bind one with `/pingroles set <dungeon>` or create \
             a fresh role with `/pingroles create <dungeon>`.",
        ))
        .await?;
        return Ok(());
    }

    // Starting state = dungeons whose bound role the user currently holds.
    let member = ctx
        .author_member()
        .await
        .ok_or_else(|| anyhow::anyhow!("could not load caller's guild membership"))?;
    let current_role_ids: HashSet<u64> = member.roles.iter().map(|r| r.get()).collect();
    let mut desired: HashSet<i32> = dungeons
        .iter()
        .filter(|d| {
            d.notification_role_id
                .map(|r| current_role_ids.contains(&(r as u64)))
                .unwrap_or(false)
        })
        .map(|d| d.id)
        .collect();
    drop(member);

    let num_pages = dungeons.len().div_ceil(PAGE_SIZE);
    let mut page: usize = 0;

    let (embed, components) = render(&dungeons, &desired, page, num_pages);
    let handle = ctx
        .send(
            CreateReply::default()
                .embed(embed)
                .components(components)
                .ephemeral(true),
        )
        .await?;
    let msg_id = handle.message().await?.id;

    loop {
        let Some(mci) = ComponentInteractionCollector::new(&ctx.serenity_context().shard)
            .message_id(msg_id)
            .author_id(ctx.author().id)
            .timeout(PICKER_TIMEOUT)
            .await
        else {
            return Ok(());
        };

        match mci.data.custom_id.as_str() {
            "pingroles:select" => {
                if let ComponentInteractionDataKind::StringSelect { values } = &mci.data.kind {
                    apply_page_selection(&dungeons, &mut desired, page, values);
                }
                respond_refresh(&ctx, &mci, &dungeons, &desired, page, num_pages).await?;
            }
            "pingroles:prev" => {
                if page > 0 {
                    page -= 1;
                }
                respond_refresh(&ctx, &mci, &dungeons, &desired, page, num_pages).await?;
            }
            "pingroles:next" => {
                if page + 1 < num_pages {
                    page += 1;
                }
                respond_refresh(&ctx, &mci, &dungeons, &desired, page, num_pages).await?;
            }
            "pingroles:apply" => {
                let result = apply_subscription_diff(ctx, &dungeons, &desired).await?;
                mci.create_response(
                    ctx.http(),
                    CreateInteractionResponse::UpdateMessage(
                        CreateInteractionResponseMessage::new()
                            .content(result)
                            .embeds(vec![])
                            .components(vec![]),
                    ),
                )
                .await?;
                return Ok(());
            }
            "pingroles:close" => {
                mci.create_response(
                    ctx.http(),
                    CreateInteractionResponse::UpdateMessage(
                        CreateInteractionResponseMessage::new()
                            .content("Closed — no changes applied.")
                            .embeds(vec![])
                            .components(vec![]),
                    ),
                )
                .await?;
                return Ok(());
            }
            _ => {
                mci.defer(ctx.http()).await?;
            }
        }
    }
}

fn apply_page_selection(
    dungeons: &[DungeonTemplate],
    desired: &mut HashSet<i32>,
    page: usize,
    values: &[String],
) {
    let start = page * PAGE_SIZE;
    let end = (start + PAGE_SIZE).min(dungeons.len());
    for d in &dungeons[start..end] {
        desired.remove(&d.id);
    }
    for v in values {
        if let Ok(id) = v.parse::<i32>() {
            if dungeons[start..end].iter().any(|d| d.id == id) {
                desired.insert(id);
            }
        }
    }
}

async fn respond_refresh(
    ctx: &BotContext<'_>,
    mci: &serenity::ComponentInteraction,
    dungeons: &[DungeonTemplate],
    desired: &HashSet<i32>,
    page: usize,
    num_pages: usize,
) -> Result<(), BotError> {
    let (embed, components) = render(dungeons, desired, page, num_pages);
    mci.create_response(
        ctx.http(),
        CreateInteractionResponse::UpdateMessage(
            CreateInteractionResponseMessage::new()
                .embed(embed)
                .components(components),
        ),
    )
    .await?;
    Ok(())
}

fn render(
    dungeons: &[DungeonTemplate],
    desired: &HashSet<i32>,
    page: usize,
    num_pages: usize,
) -> (CreateEmbed, Vec<CreateActionRow>) {
    let start = page * PAGE_SIZE;
    let end = (start + PAGE_SIZE).min(dungeons.len());
    let page_slice = &dungeons[start..end];

    let options: Vec<CreateSelectMenuOption> = page_slice
        .iter()
        .map(|d| {
            CreateSelectMenuOption::new(d.display_name.clone(), d.id.to_string())
                .default_selection(desired.contains(&d.id))
        })
        .collect();

    let max_values = page_slice.len().max(1) as u8;
    let select = CreateSelectMenu::new(
        "pingroles:select",
        CreateSelectMenuKind::String { options },
    )
    .placeholder("Select dungeons to subscribe to")
    .min_values(0)
    .max_values(max_values);

    let buttons = CreateActionRow::Buttons(vec![
        CreateButton::new("pingroles:prev")
            .label("◀ Prev")
            .style(ButtonStyle::Secondary)
            .disabled(page == 0),
        CreateButton::new("pingroles:next")
            .label("Next ▶")
            .style(ButtonStyle::Secondary)
            .disabled(page + 1 >= num_pages),
        CreateButton::new("pingroles:apply")
            .label("Apply")
            .style(ButtonStyle::Success),
        CreateButton::new("pingroles:close")
            .label("Cancel")
            .style(ButtonStyle::Secondary),
    ]);

    let summary = if desired.is_empty() {
        "Currently subscribed to no dungeons.".to_string()
    } else {
        format!("Currently subscribed to **{}** dungeon(s).", desired.len())
    };

    let embed = CreateEmbed::new()
        .title("🔔 Dungeon notifications")
        .description(format!(
            "Tick the dungeons you want to be pinged for. Untick to unsubscribe. \
             Click **Apply** to save.\n\
             \n\
             Page **{}/{}** · {summary}",
            page + 1,
            num_pages.max(1),
        ))
        .color(0x5865F2);

    (embed, vec![CreateActionRow::SelectMenu(select), buttons])
}

async fn apply_subscription_diff(
    ctx: BotContext<'_>,
    dungeons: &[DungeonTemplate],
    desired: &HashSet<i32>,
) -> Result<String, BotError> {
    let http = ctx.http();
    let guild_id = ctx.guild_id().unwrap();
    let user_id = ctx.author().id;

    // Role IDs in Starship's "managed" set (only these are touched — unrelated
    // roles the user holds stay untouched).
    let managed_roles: HashSet<u64> = dungeons
        .iter()
        .filter_map(|d| d.notification_role_id.map(|r| r as u64))
        .collect();

    let desired_roles: HashSet<u64> = dungeons
        .iter()
        .filter(|d| desired.contains(&d.id))
        .filter_map(|d| d.notification_role_id.map(|r| r as u64))
        .collect();

    // Fetch fresh member state so we diff against truth, not the
    // possibly-stale cached copy from the initial render.
    let member = guild_id.member(http, user_id).await?;
    let current_roles: HashSet<u64> = member.roles.iter().map(|r| r.get()).collect();

    let to_add: Vec<u64> = desired_roles.difference(&current_roles).copied().collect();
    let to_remove: Vec<u64> = current_roles
        .intersection(&managed_roles)
        .filter(|r| !desired_roles.contains(r))
        .copied()
        .collect();

    let mut failures: Vec<String> = Vec::new();

    for rid in &to_add {
        if let Err(e) = mutate_role_with_retry(ctx, &member, *rid, RoleOp::Add).await {
            failures.push(format!("add <@&{rid}>: {e}"));
        }
    }
    for rid in &to_remove {
        if let Err(e) = mutate_role_with_retry(ctx, &member, *rid, RoleOp::Remove).await {
            failures.push(format!("remove <@&{rid}>: {e}"));
        }
    }

    let added_ok = to_add.len() - failures.iter().filter(|f| f.starts_with("add ")).count();
    let removed_ok =
        to_remove.len() - failures.iter().filter(|f| f.starts_with("remove ")).count();

    if failures.is_empty() {
        if added_ok == 0 && removed_ok == 0 {
            Ok("Nothing to change — your subscriptions already match.".to_string())
        } else {
            Ok(format!(
                "✅ Subscriptions updated — {added_ok} added, {removed_ok} removed."
            ))
        }
    } else {
        Ok(format!(
            "⚠ Partial update: {added_ok} added, {removed_ok} removed. \
             {} failure(s):\n{}",
            failures.len(),
            failures.join("\n"),
        ))
    }
}

#[derive(Copy, Clone)]
enum RoleOp {
    Add,
    Remove,
}

async fn mutate_role_with_retry(
    ctx: BotContext<'_>,
    member: &serenity::Member,
    role_id: u64,
    op: RoleOp,
) -> Result<(), BotError> {
    let http = ctx.http();
    let rid = RoleId::new(role_id);

    let mut last: Option<BotError> = None;
    for attempt in 0..ROLE_MUTATION_ATTEMPTS {
        let res = match op {
            RoleOp::Add => member.add_role(http, rid).await,
            RoleOp::Remove => member.remove_role(http, rid).await,
        };
        match res {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e.into());
                // Brief backoff before the next attempt — Discord rate-limits
                // burst role mutations. 200→400→800 ms is enough for most
                // transient spikes without making the user wait forever.
                if attempt + 1 < ROLE_MUTATION_ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(200 << attempt)).await;
                }
            }
        }
    }
    Err(last.expect("at least one attempt was made"))
}

// ---------------------------------------------------------------------------
// Admin subcommands
// ---------------------------------------------------------------------------

/// Bind an existing Discord role to a dungeon.
#[poise::command(slash_command, guild_only, rename = "set")]
pub async fn set_(
    ctx: BotContext<'_>,
    #[description = "Dungeon to bind"]
    #[autocomplete = "autocomplete_dungeon"]
    dungeon: String,
    #[description = "Role pinged when a headcount/run starts"]
    role: serenity::Role,
) -> Result<(), BotError> {
    perm_svc::require(ctx, Action::ConfigureGuild, None, None).await?;

    let guild_id = ctx.guild_id().unwrap().get() as i64;
    let pool = &ctx.data().db;

    let Some(template) = db::dungeon::get_by_name(pool, guild_id, &dungeon).await? else {
        ctx.send(ephemeral(format!("Unknown dungeon `{dungeon}`.")))
            .await?;
        return Ok(());
    };

    db::dungeon::set_notification_role(pool, guild_id, template.id, Some(role.id.get() as i64))
        .await?;

    ctx.send(ephemeral(format!(
        "Bound <@&{}> as the notification role for **{}**.",
        role.id, template.display_name,
    )))
    .await?;
    Ok(())
}

/// Clear the notification role binding for a dungeon.
#[poise::command(slash_command, guild_only)]
pub async fn unset(
    ctx: BotContext<'_>,
    #[description = "Dungeon to clear"]
    #[autocomplete = "autocomplete_dungeon"]
    dungeon: String,
) -> Result<(), BotError> {
    perm_svc::require(ctx, Action::ConfigureGuild, None, None).await?;

    let guild_id = ctx.guild_id().unwrap().get() as i64;
    let pool = &ctx.data().db;

    let Some(template) = db::dungeon::get_by_name(pool, guild_id, &dungeon).await? else {
        ctx.send(ephemeral(format!("Unknown dungeon `{dungeon}`.")))
            .await?;
        return Ok(());
    };

    db::dungeon::set_notification_role(pool, guild_id, template.id, None).await?;

    ctx.send(ephemeral(format!(
        "Cleared the notification role for **{}**.",
        template.display_name,
    )))
    .await?;
    Ok(())
}

/// Create a fresh mentionable role and bind it to a dungeon.
#[poise::command(slash_command, guild_only)]
pub async fn create(
    ctx: BotContext<'_>,
    #[description = "Dungeon to bind"]
    #[autocomplete = "autocomplete_dungeon"]
    dungeon: String,
) -> Result<(), BotError> {
    perm_svc::require(ctx, Action::ConfigureGuild, None, None).await?;

    let guild_id_struct = ctx.guild_id().unwrap();
    let guild_id = guild_id_struct.get() as i64;
    let pool = &ctx.data().db;
    let http = ctx.http();

    let Some(template) = db::dungeon::get_by_name(pool, guild_id, &dungeon).await? else {
        ctx.send(ephemeral(format!("Unknown dungeon `{dungeon}`.")))
            .await?;
        return Ok(());
    };

    let role_name = format!("{} Pings", template.display_name);

    // If a role with this name already exists, reuse it rather than creating
    // a duplicate.
    let role_id = match guild_id_struct
        .roles(http)
        .await?
        .iter()
        .find(|(_, r)| r.name.eq_ignore_ascii_case(&role_name))
        .map(|(id, _)| *id)
    {
        Some(id) => id,
        None => {
            let role = guild_id_struct
                .create_role(
                    http,
                    EditRole::new()
                        .name(&role_name)
                        .permissions(Permissions::empty())
                        .mentionable(true)
                        .hoist(false),
                )
                .await?;
            role.id
        }
    };

    db::dungeon::set_notification_role(pool, guild_id, template.id, Some(role_id.get() as i64))
        .await?;

    ctx.send(ephemeral(format!(
        "Created (or reused) <@&{}> and bound it as the notification role for **{}**. \
         Users can now run `/pingroles` to subscribe.",
        role_id, template.display_name,
    )))
    .await?;
    Ok(())
}
