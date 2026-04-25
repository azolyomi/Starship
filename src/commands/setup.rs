use std::time::Duration;

use anyhow::Result;
use poise::serenity_prelude as serenity;
use poise::{CreateReply, ReplyHandle};
use serenity::{
    ButtonStyle, ChannelId, ChannelType, ComponentInteraction, ComponentInteractionCollector,
    ComponentInteractionDataKind, CreateActionRow, CreateButton, CreateChannel, CreateEmbed,
    CreateEmbedFooter, CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage,
    CreateSelectMenu, CreateSelectMenuKind, EditInteractionResponse, EditRole, MessageId,
    PermissionOverwrite, PermissionOverwriteType, Permissions, RoleId, UserId,
};

use crate::db::models::Tier;
use crate::{db, guild_id_i64, require_guild_id, services::permission, BotContext, BotError};

/// How long to wait for a click before the wizard expires.
const WIZARD_TIMEOUT: Duration = Duration::from_secs(600);

// ---------------------------------------------------------------------------
// Command entry
// ---------------------------------------------------------------------------

/// Configure Starship for this server. Re-run any time to change settings.
#[poise::command(slash_command, guild_only, default_member_permissions = "MANAGE_GUILD")]
pub async fn setup(ctx: BotContext<'_>) -> Result<(), BotError> {
    permission::require_discord_admin(ctx).await?;

    let guild_id = guild_id_i64(ctx);

    // Make sure the guild row exists before any downstream FK target is needed.
    db::guild::upsert(&ctx.data().db, guild_id).await?;

    let guild_row = db::guild::get(&ctx.data().db, guild_id)
        .await?
        .expect("guild row upserted immediately above");

    // Servers that have already finished setup skip the intro screen —
    // `/setup` just reopens the dashboard for tweaks.
    if guild_row.setup_complete {
        let (embed, components) = dashboard_view(ctx).await?;
        let handle = ctx
            .send(
                CreateReply::default()
                    .embed(embed)
                    .components(components)
                    .ephemeral(true),
            )
            .await?;
        let msg_id = handle.message().await?.id;
        return run_dashboard_loop(ctx, &handle, msg_id).await;
    }

    // Fresh server — show a quick/custom chooser first.
    let (embed, components) = intro_view();
    let handle = ctx
        .send(
            CreateReply::default()
                .embed(embed)
                .components(components)
                .ephemeral(true),
        )
        .await?;
    let msg_id = handle.message().await?.id;

    let Some(mci) = await_next(ctx, msg_id).await else {
        handle
            .edit(
                ctx,
                CreateReply::default()
                    .content("Setup wizard timed out. Run `/setup` again.")
                    .components(vec![]),
            )
            .await?;
        return Ok(());
    };

    match mci.data.custom_id.as_str() {
        "setup:intro:quick" => quick_setup(ctx, &mci).await,
        "setup:intro:custom" => {
            let (embed, components) = dashboard_view(ctx).await?;
            respond_with_view(ctx, &mci, embed, components).await?;
            run_dashboard_loop(ctx, &handle, msg_id).await
        }
        "setup:intro:close" => {
            respond_plain(ctx, &mci, "Wizard closed. Run `/setup` any time.").await?;
            Ok(())
        }
        _ => {
            mci.defer(ctx.http()).await?;
            Ok(())
        }
    }
}

/// Main dashboard event loop, extracted so it can be entered from either the
/// intro screen's "Custom setup" button or directly for already-set-up guilds.
async fn run_dashboard_loop(
    ctx: BotContext<'_>,
    handle: &ReplyHandle<'_>,
    msg_id: MessageId,
) -> Result<(), BotError> {
    let guild_id = guild_id_i64(ctx);

    loop {
        let Some(mci) = await_next(ctx, msg_id).await else {
            handle
                .edit(
                    ctx,
                    CreateReply::default()
                        .content(
                            "Setup wizard timed out. Run `/setup` again to resume \
                             — your progress is saved.",
                        )
                        .components(vec![]),
                )
                .await?;
            return Ok(());
        };

        match mci.data.custom_id.as_str() {
            "setup:cancel" => {
                respond_plain(
                    ctx,
                    &mci,
                    "Wizard closed. Run `/setup` again any time to change settings.",
                )
                .await?;
                return Ok(());
            }
            "setup:finish" => {
                db::guild::mark_setup_complete(&ctx.data().db, guild_id, true).await?;
                let embed = summary_view(ctx).await?;
                respond_with_view(ctx, &mci, embed, vec![]).await?;
                return Ok(());
            }
            "setup:section:tier" => section_first_tier(ctx, &mci).await?,
            "setup:section:superadmin" => section_superadmin(ctx, &mci).await?,
            "setup:section:log" => section_log_channel(ctx, &mci).await?,
            "setup:section:verify" => section_verification(ctx, &mci).await?,
            _ => {
                // Unknown custom_id — just acknowledge so Discord doesn't
                // show "interaction failed" to the user.
                mci.defer(ctx.http()).await?;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Intro / Quick setup
// ---------------------------------------------------------------------------

fn intro_view() -> (CreateEmbed, Vec<CreateActionRow>) {
    let embed = CreateEmbed::new()
        .title("🛠 Starship setup")
        .description(
            "Welcome! How would you like to set up?\n\
             \n\
             **Quick setup** — one click, sensible defaults:\n\
             • Creates a **Raids** category with a single `main-runs` \
               channel (headcounts and runs both live there)\n\
             • Creates `#🚀starship-log` for audit events\n\
             • Creates a **Raid Leader** role with raid-management \
               permissions\n\
             • Makes you the superadmin\n\
             \n\
             **Custom setup** — pick every channel and role yourself via a \
             dashboard. Best if you already have a channel structure.",
        )
        .color(0x5865F2)
        .footer(CreateEmbedFooter::new(
            "You can re-run `/setup` any time to change anything.",
        ));

    let buttons = CreateActionRow::Buttons(vec![
        CreateButton::new("setup:intro:quick")
            .label("Quick setup")
            .style(ButtonStyle::Success),
        CreateButton::new("setup:intro:custom")
            .label("Custom setup")
            .style(ButtonStyle::Primary),
        CreateButton::new("setup:intro:close")
            .label("Close")
            .style(ButtonStyle::Secondary),
    ]);

    (embed, vec![buttons])
}

/// One-click default provisioning. Creates channels, a Raid Leader role with
/// raid-management permissions, assigns the invoking user as superadmin, and
/// marks setup complete. Idempotent on re-entry — re-uses existing channels /
/// roles when they already match the expected names.
async fn quick_setup(ctx: BotContext<'_>, trigger: &ComponentInteraction) -> Result<(), BotError> {
    // Acknowledge silently so we can take >3s on HTTP calls without the
    // button showing "interaction failed".
    trigger
        .create_response(ctx.http(), CreateInteractionResponse::Acknowledge)
        .await?;

    match do_quick_setup(ctx).await {
        Ok(()) => {
            let summary = summary_view(ctx).await?;
            trigger
                .edit_response(
                    ctx.http(),
                    EditInteractionResponse::new()
                        .embed(summary)
                        .components(vec![]),
                )
                .await?;
        }
        Err(e) => {
            tracing::warn!(
                error = ?e,
                guild_id = ctx.guild_id().map(|g| g.get()),
                "quick setup failed",
            );
            trigger
                .edit_response(
                    ctx.http(),
                    EditInteractionResponse::new()
                        .content(format!(
                            "⚠ Quick setup failed: {e}\n\n\
                             Make sure I have **Manage Channels** and **Manage Roles** \
                             permissions, then run `/setup` again. Or try **Custom setup** \
                             to configure things by hand."
                        ))
                        .embeds(vec![])
                        .components(vec![]),
                )
                .await?;
        }
    }

    Ok(())
}

async fn do_quick_setup(ctx: BotContext<'_>) -> Result<()> {
    let guild_id = guild_id_i64(ctx);
    let pool = &ctx.data().db;
    let user_id = ctx.author().id.get() as i64;

    // Single runs channel under a Raids category (R3: no more split
    // headcount / raid channels).
    let runs_id = create_default_channels(ctx, "Main").await?;

    // Log channel — emoji-prefixed name, fallback to plain if Discord
    // rejects the leading rocket glyph.
    let log_id = find_or_create_log_channel(ctx).await?;

    // Main tier.
    let existing_main = db::tier::list(pool, guild_id)
        .await?
        .into_iter()
        .find(|t| t.name == "Main");
    let tier_id = match existing_main {
        Some(t) => {
            db::tier::update(pool, t.id, None, None, Some(runs_id.get() as i64)).await?;
            t.id
        }
        None => {
            let created = db::tier::create(pool, guild_id, "Main", None).await?;
            db::tier::update(pool, created.id, None, None, Some(runs_id.get() as i64)).await?;
            // Attach every global dungeon so `/headcount` works out of the box.
            for d in db::dungeon::list_for_guild(pool, guild_id).await? {
                db::tier::add_dungeon(pool, created.id, d.id).await?;
            }
            created.id
        }
    };
    // Leave tier access roles empty = anyone in the server can participate.
    let _ = tier_id;

    db::guild::set_log_channel(pool, guild_id, Some(log_id.get() as i64)).await?;
    db::guild::set_superadmin(pool, guild_id, Some(user_id)).await?;

    // Raid Leader role + raid-management permission grants.
    let role_id = find_or_create_raid_leader_role(ctx).await?;
    for action in [
        "StartHeadcount",
        "ConvertHeadcount",
        "CancelHeadcount",
        "StartRun",
        "EndRun",
        "ManageRuns",
        "CreateVcRaid",
    ] {
        db::permission::grant(pool, guild_id, role_id.get() as i64, action, None, None).await?;
    }

    // Verification: a role for verified users + a channel hosting the
    // persistent Verify button + the button message itself. Wired
    // together by storing the three IDs on the guild row.
    let verified_role_id = find_or_create_verified_role(ctx).await?;
    let verify_channel_id = find_or_create_verify_channel(ctx, verified_role_id).await?;
    let verify_message_id = find_or_post_verify_message(ctx, verify_channel_id, None).await?;
    db::guild::set_verified_role(pool, guild_id, Some(verified_role_id.get() as i64)).await?;
    db::guild::set_verify_channel(pool, guild_id, Some(verify_channel_id.get() as i64)).await?;
    db::guild::set_verify_message(pool, guild_id, Some(verify_message_id.get() as i64)).await?;

    db::guild::mark_setup_complete(pool, guild_id, true).await?;
    Ok(())
}

/// Find-or-create the Starship audit-log channel. Prefers the
/// emoji-prefixed `🚀starship-log` name for discoverability in the channel
/// list; falls back to plain `starship-log` if Discord rejects the glyph
/// (some guild settings / old clients choke on leading emoji).
async fn find_or_create_log_channel(ctx: BotContext<'_>) -> Result<ChannelId> {
    const FANCY: &str = "🚀starship-log";
    const PLAIN: &str = "starship-log";

    let guild_id = require_guild_id(ctx);
    let http = ctx.http();
    let existing = guild_id.channels(http).await?;

    for name in [FANCY, PLAIN] {
        if let Some(c) = existing
            .values()
            .find(|c| c.kind == ChannelType::Text && c.name.eq_ignore_ascii_case(name))
        {
            return Ok(c.id);
        }
    }

    match guild_id
        .create_channel(http, CreateChannel::new(FANCY).kind(ChannelType::Text))
        .await
    {
        Ok(ch) => Ok(ch.id),
        Err(e) => {
            tracing::warn!(error = ?e, "log channel with emoji prefix rejected, falling back");
            Ok(guild_id
                .create_channel(http, CreateChannel::new(PLAIN).kind(ChannelType::Text))
                .await?
                .id)
        }
    }
}

/// Find-or-create a "Verified" role. No Discord permissions — it's a
/// flag the bot reads, not a permission grant. Idempotent.
async fn find_or_create_verified_role(ctx: BotContext<'_>) -> Result<RoleId> {
    let guild_id = require_guild_id(ctx);
    let http = ctx.http();

    let roles = guild_id.roles(http).await?;
    if let Some((id, _)) = roles
        .iter()
        .find(|(_, r)| r.name.eq_ignore_ascii_case("Verified"))
    {
        return Ok(*id);
    }

    let role = guild_id
        .create_role(
            http,
            EditRole::new()
                .name("Verified")
                .permissions(Permissions::empty())
                .mentionable(false)
                .hoist(false),
        )
        .await?;
    Ok(role.id)
}

/// Find-or-create the verification channel. Prefers the emoji-prefixed
/// `🔐verify` name; falls back to plain `verify` if Discord rejects the
/// glyph (some guild settings choke on leading emoji). Permission
/// overwrites:
///
/// * `@everyone` may read history but cannot send messages — the only
///   interaction is the persistent Verify button.
/// * The verified role's view is denied so already-verified users don't
///   see the channel cluttering their list. They never need to come
///   back.
async fn find_or_create_verify_channel(
    ctx: BotContext<'_>,
    verified_role_id: RoleId,
) -> Result<ChannelId> {
    const FANCY: &str = "🔐verify";
    const PLAIN: &str = "verify";

    let guild_id = require_guild_id(ctx);
    let http = ctx.http();
    let existing = guild_id.channels(http).await?;

    for name in [FANCY, PLAIN] {
        if let Some(c) = existing
            .values()
            .find(|c| c.kind == ChannelType::Text && c.name.eq_ignore_ascii_case(name))
        {
            return Ok(c.id);
        }
    }

    // Permission overwrites baked into the create call so the channel is
    // born locked-down. Server admins can edit overwrites later if they
    // want different visibility — `/setup` doesn't enforce them on each
    // re-run.
    let everyone_role_id = RoleId::new(guild_id.get());
    let overwrites = verify_channel_overwrites(everyone_role_id, verified_role_id);

    let create = CreateChannel::new(FANCY)
        .kind(ChannelType::Text)
        .permissions(overwrites.clone());
    match guild_id.create_channel(http, create).await {
        Ok(ch) => Ok(ch.id),
        Err(e) => {
            tracing::warn!(error = ?e, "verify channel with emoji prefix rejected, falling back");
            Ok(guild_id
                .create_channel(
                    http,
                    CreateChannel::new(PLAIN)
                        .kind(ChannelType::Text)
                        .permissions(overwrites),
                )
                .await?
                .id)
        }
    }
}

fn verify_channel_overwrites(
    everyone_role_id: RoleId,
    verified_role_id: RoleId,
) -> Vec<PermissionOverwrite> {
    vec![
        PermissionOverwrite {
            allow: Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY,
            deny: Permissions::SEND_MESSAGES
                | Permissions::ADD_REACTIONS
                | Permissions::CREATE_PUBLIC_THREADS
                | Permissions::CREATE_PRIVATE_THREADS,
            kind: PermissionOverwriteType::Role(everyone_role_id),
        },
        // Verified users don't need to see the channel anymore. Hiding
        // it reduces noise and discourages re-verification spam.
        PermissionOverwrite {
            allow: Permissions::empty(),
            deny: Permissions::VIEW_CHANNEL,
            kind: PermissionOverwriteType::Role(verified_role_id),
        },
    ]
}

/// Find-or-post the persistent Verify button message. If `existing_id`
/// is provided and the message still exists, returns it as-is. Otherwise
/// posts a fresh message and returns its ID. The caller persists the
/// returned ID on the guild row.
async fn find_or_post_verify_message(
    ctx: BotContext<'_>,
    channel_id: ChannelId,
    existing_id: Option<MessageId>,
) -> Result<MessageId> {
    let http = ctx.http();
    if let Some(id) = existing_id {
        if channel_id.message(http, id).await.is_ok() {
            return Ok(id);
        }
        // 404 (or any error) → fall through and post a new one.
    }

    let embed = verify_button_embed();
    let buttons = CreateActionRow::Buttons(vec![CreateButton::new("verify:start")
        .label("Verify")
        .style(ButtonStyle::Success)]);
    let msg = channel_id
        .send_message(
            http,
            CreateMessage::new().embed(embed).components(vec![buttons]),
        )
        .await?;
    Ok(msg.id)
}

/// The persistent Verify-button message body. Mirrors the public
/// verification scripts most RotMG halls use — the user pastes a code
/// into their RealmEye description; the bot scrapes it back. Kept as
/// one function so the wording stays consistent between quick-setup
/// posting, custom-setup repost, and any future restart-recovery post.
fn verify_button_embed() -> CreateEmbed {
    CreateEmbed::new()
        .title("🔐 Get verified")
        .description(
            "Verification links your Discord account to your in-game name (IGN) \
             via your RealmEye profile.\n\
             \n\
             **How it works**\n\
             1. Click **Verify** below and enter your IGN.\n\
             2. The bot will reply with a 6-digit code (only you can see it).\n\
             3. Log in to <https://www.realmeye.com>, open your profile, and \
                paste the code into your description. Save.\n\
             4. Come back here, click **I added it**, and you're done — \
                you'll get the **Verified** role and your nickname will be \
                set to your IGN.\n\
             \n\
             **Trouble?**\n\
             • Make sure your RealmEye description is set to public.\n\
             • Make sure you've logged into the game on this account at \
                least once recently — RealmEye won't show characters \
                that have never been seen.\n\
             • If verification keeps failing, ask a moderator to verify \
                you manually.",
        )
        .color(0x57F287)
}

/// Find-or-create a "Raid Leader" role. The role itself has no Discord
/// permissions — Starship's permission checks happen in-bot. Idempotent.
async fn find_or_create_raid_leader_role(ctx: BotContext<'_>) -> Result<RoleId> {
    let guild_id = require_guild_id(ctx);
    let http = ctx.http();

    let roles = guild_id.roles(http).await?;
    if let Some((id, _)) = roles
        .iter()
        .find(|(_, r)| r.name.eq_ignore_ascii_case("Raid Leader"))
    {
        return Ok(*id);
    }

    let role = guild_id
        .create_role(
            http,
            EditRole::new()
                .name("Raid Leader")
                .permissions(Permissions::empty())
                .mentionable(true)
                .hoist(false),
        )
        .await?;
    Ok(role.id)
}

// ---------------------------------------------------------------------------
// Dashboard view
// ---------------------------------------------------------------------------

async fn dashboard_view(ctx: BotContext<'_>) -> Result<(CreateEmbed, Vec<CreateActionRow>)> {
    let guild_id = guild_id_i64(ctx);
    let guild = db::guild::get(&ctx.data().db, guild_id)
        .await?
        .expect("guild row upserted in `setup`");
    let tiers = db::tier::list(&ctx.data().db, guild_id).await?;

    let guild_name = ctx
        .guild()
        .map(|g| g.name.clone())
        .unwrap_or_else(|| "this server".to_string());

    let first_tier = tiers.first();
    let first_tier_ready = first_tier
        .map(|t| t.runs_channel_id.is_some())
        .unwrap_or(false);

    let mark = |ok: bool| if ok { "✅" } else { "⬜" };

    let tier_block = match first_tier {
        Some(t) => {
            let runs = t
                .runs_channel_id
                .map(|c| format!("<#{c}>"))
                .unwrap_or_else(|| "_no runs channel_".to_string());
            let extra = if tiers.len() > 1 {
                format!(
                    "\n_+ {} more tier(s) — manage with `/tier`._",
                    tiers.len() - 1
                )
            } else {
                String::new()
            };
            format!("**{}** — runs: {runs}{extra}", t.name)
        }
        None => "_no tiers yet — set one up to enable **Finish**_".to_string(),
    };

    let sa = guild
        .superadmin_user_id
        .map(|uid| format!("<@{uid}>"))
        .unwrap_or_else(|| "_not set (Discord admins still have full access)_".to_string());
    let log = guild
        .log_channel_id
        .map(|cid| format!("<#{cid}>"))
        .unwrap_or_else(|| "_not set_".to_string());

    // Verification: ✅ requires role + channel + posted message all
    // present. Anything less and the persistent button won't be visible
    // to users.
    let verify_ready = guild.verified_role_id.is_some()
        && guild.verify_channel_id.is_some()
        && guild.verify_message_id.is_some();
    let verify_block = match (
        guild.verified_role_id,
        guild.verify_channel_id,
        guild.verify_message_id,
    ) {
        (Some(role), Some(chan), Some(_)) => format!("role <@&{role}> in <#{chan}>"),
        (Some(role), Some(chan), None) => {
            format!("role <@&{role}> in <#{chan}> — _Verify message not posted yet_")
        }
        (Some(role), None, _) => format!("role <@&{role}> — _no channel set_"),
        (None, Some(chan), _) => format!("<#{chan}> — _no role set_"),
        (None, None, _) => "_not set_".to_string(),
    };

    let description = format!(
        "Configure Starship for **{guild_name}**. Click a section to edit.\n\
         \n\
         {tier_mark} **First tier** *(required)*\n\
         {tier_block}\n\
         \n\
         {sa_mark} **Superadmin** *(bypass for emergencies)*\n\
         {sa}\n\
         \n\
         {log_mark} **Audit log channel** *(optional)*\n\
         {log}\n\
         \n\
         {verify_mark} **Verification** *(optional)*\n\
         {verify_block}",
        tier_mark = mark(first_tier_ready),
        sa_mark = mark(guild.superadmin_user_id.is_some()),
        log_mark = mark(guild.log_channel_id.is_some()),
        verify_mark = mark(verify_ready),
    );

    let footer = if guild.setup_complete {
        CreateEmbedFooter::new("✨ Already set up — tweak any section and Save & close.")
    } else if first_tier_ready {
        CreateEmbedFooter::new("Ready — click Finish to lock in the configuration.")
    } else {
        CreateEmbedFooter::new("Set up the first tier to enable Finish.")
    };

    let embed = CreateEmbed::new()
        .title("🛠 Starship setup")
        .description(description)
        .color(0x5865F2)
        .footer(footer);

    let tier_label = if first_tier.is_some() {
        "Edit first tier"
    } else {
        "Set up first tier"
    };
    let tier_style = if first_tier_ready {
        ButtonStyle::Secondary
    } else {
        ButtonStyle::Primary
    };

    let verify_style = if verify_ready {
        ButtonStyle::Secondary
    } else {
        ButtonStyle::Primary
    };

    let sections_row = CreateActionRow::Buttons(vec![
        CreateButton::new("setup:section:tier")
            .label(tier_label)
            .style(tier_style),
        CreateButton::new("setup:section:superadmin")
            .label("Superadmin")
            .style(ButtonStyle::Secondary),
        CreateButton::new("setup:section:log")
            .label("Log channel")
            .style(ButtonStyle::Secondary),
        CreateButton::new("setup:section:verify")
            .label("Verification")
            .style(verify_style),
    ]);

    let finish_label = if guild.setup_complete {
        "Save & close"
    } else {
        "Finish setup"
    };
    let controls_row = CreateActionRow::Buttons(vec![
        CreateButton::new("setup:finish")
            .label(finish_label)
            .style(ButtonStyle::Success)
            .disabled(!first_tier_ready),
        CreateButton::new("setup:cancel")
            .label("Close")
            .style(ButtonStyle::Secondary),
    ]);

    Ok((embed, vec![sections_row, controls_row]))
}

/// Shown once the wizard is complete. No components — just a friendly
/// landing card with next steps.
async fn summary_view(ctx: BotContext<'_>) -> Result<CreateEmbed> {
    let guild_id = guild_id_i64(ctx);
    let tiers = db::tier::list(&ctx.data().db, guild_id).await?;
    let first = tiers
        .first()
        .expect("finish is only reachable with at least one tier");

    let runs = first
        .runs_channel_id
        .map(|c| format!("<#{c}>"))
        .unwrap_or_else(|| "_not set_".to_string());

    let dungeon_count = db::tier::list_dungeons(&ctx.data().db, first.id)
        .await?
        .len();

    let description = format!(
        "Starship is ready to raid.\n\
         \n\
         **{}** is live in {}.\n\
         {dungeon_count} dungeon(s) are attached to this tier.\n\
         \n\
         **Try it out**\n\
         • Run `/headcount <dungeon>` to start gathering raiders.\n\
         • Run `/run <dungeon>` to skip the headcount and jump straight in.\n\
         • Run `/pingroles` to subscribe to dungeon notifications.\n\
         \n\
         **Manage later**\n\
         • `/tier` — add more tiers, change channels, assign access roles\n\
         • `/permission` — let specific roles run headcounts and runs\n\
         • `/dungeon` — customise or add dungeons\n\
         • `/pingroles set` — bind a dungeon to a notification role\n\
         • `/setup` — re-run this wizard any time",
        first.name, runs
    );

    Ok(CreateEmbed::new()
        .title("✨ Setup complete")
        .description(description)
        .color(0x57F287))
}

// ---------------------------------------------------------------------------
// Section: first tier
// ---------------------------------------------------------------------------

/// In-memory draft while editing the first-tier section. Persisted to DB
/// only when the user clicks Save.
#[derive(Debug, Clone)]
struct TierDraft {
    runs_channel: Option<ChannelId>,
    access_roles: Vec<RoleId>,
}

async fn section_first_tier(
    ctx: BotContext<'_>,
    trigger: &ComponentInteraction,
) -> Result<(), BotError> {
    let guild_id = guild_id_i64(ctx);
    let pool = &ctx.data().db;

    let tiers = db::tier::list(pool, guild_id).await?;
    let existing = tiers.into_iter().next();

    let mut draft = match &existing {
        Some(t) => {
            let roles = db::tier::list_roles(pool, t.id).await?;
            TierDraft {
                runs_channel: t.runs_channel_id.map(|id| ChannelId::new(id as u64)),
                access_roles: roles.into_iter().map(|r| RoleId::new(r as u64)).collect(),
            }
        }
        None => TierDraft {
            runs_channel: None,
            access_roles: Vec::new(),
        },
    };

    let global_dungeons = db::dungeon::list_for_guild(pool, guild_id).await?;

    let (embed, components) = tier_view(existing.as_ref(), &draft, global_dungeons.len());
    respond_with_view(ctx, trigger, embed, components).await?;

    let msg_id = trigger.message.id;
    loop {
        let Some(mci) = await_next(ctx, msg_id).await else {
            return Ok(());
        };

        match mci.data.custom_id.as_str() {
            "setup:tier:back" => {
                back_to_dashboard(ctx, &mci).await?;
                return Ok(());
            }
            "setup:tier:runs" => {
                if let ComponentInteractionDataKind::ChannelSelect { values } = &mci.data.kind {
                    draft.runs_channel = values.first().copied();
                }
                let (embed, components) =
                    tier_view(existing.as_ref(), &draft, global_dungeons.len());
                respond_with_view(ctx, &mci, embed, components).await?;
            }
            "setup:tier:roles" => {
                if let ComponentInteractionDataKind::RoleSelect { values } = &mci.data.kind {
                    draft.access_roles = values.clone();
                }
                let (embed, components) =
                    tier_view(existing.as_ref(), &draft, global_dungeons.len());
                respond_with_view(ctx, &mci, embed, components).await?;
            }
            "setup:tier:create_channels" => {
                let tier_name = existing.as_ref().map(|t| t.name.as_str()).unwrap_or("Main");
                match create_default_channels(ctx, tier_name).await {
                    Ok(runs_id) => {
                        draft.runs_channel = Some(runs_id);
                        let (embed, components) =
                            tier_view(existing.as_ref(), &draft, global_dungeons.len());
                        respond_with_view(ctx, &mci, embed, components).await?;
                    }
                    Err(e) => {
                        tracing::warn!(error = ?e, "tier channel creation failed");
                        mci.create_response(
                            ctx.http(),
                            CreateInteractionResponse::Message(
                                CreateInteractionResponseMessage::new()
                                    .content(format!(
                                        "⚠ Couldn't create channels: {e}\n\
                                         Make sure I have the **Manage Channels** permission."
                                    ))
                                    .ephemeral(true),
                            ),
                        )
                        .await?;
                    }
                }
            }
            "setup:tier:save" => {
                let Some(runs) = draft.runs_channel else {
                    // Save button is disabled without a runs channel, but be defensive.
                    mci.defer(ctx.http()).await?;
                    continue;
                };

                let tier_id = match &existing {
                    Some(t) => {
                        db::tier::update(pool, t.id, None, None, Some(runs.get() as i64)).await?;
                        t.id
                    }
                    None => {
                        let created = db::tier::create(pool, guild_id, "Main", None).await?;
                        db::tier::update(pool, created.id, None, None, Some(runs.get() as i64))
                            .await?;
                        // Magical default: attach every globally-available
                        // dungeon so `/headcount` works out of the box.
                        for d in &global_dungeons {
                            db::tier::add_dungeon(pool, created.id, d.id).await?;
                        }
                        created.id
                    }
                };

                sync_roles(pool, tier_id, &draft.access_roles).await?;

                back_to_dashboard(ctx, &mci).await?;
                return Ok(());
            }
            _ => {
                mci.defer(ctx.http()).await?;
            }
        }
    }
}

async fn sync_roles(pool: &sqlx::PgPool, tier_id: i32, desired: &[RoleId]) -> Result<()> {
    let existing: std::collections::HashSet<i64> = db::tier::list_roles(pool, tier_id)
        .await?
        .into_iter()
        .collect();
    let desired_set: std::collections::HashSet<i64> =
        desired.iter().map(|r| r.get() as i64).collect();

    for add in desired_set.difference(&existing) {
        db::tier::add_role(pool, tier_id, *add).await?;
    }
    for remove in existing.difference(&desired_set) {
        db::tier::remove_role(pool, tier_id, *remove).await?;
    }
    Ok(())
}

fn tier_view(
    existing: Option<&Tier>,
    draft: &TierDraft,
    dungeon_count: usize,
) -> (CreateEmbed, Vec<CreateActionRow>) {
    let name = existing.map(|t| t.name.as_str()).unwrap_or("Main");
    let is_create = existing.is_none();

    let runs_display = draft
        .runs_channel
        .map(|c| format!("<#{c}>"))
        .unwrap_or_else(|| "_required — pick one below_".to_string());
    let roles_display = if draft.access_roles.is_empty() {
        "_anyone in the server_".to_string()
    } else {
        draft
            .access_roles
            .iter()
            .map(|r| format!("<@&{r}>"))
            .collect::<Vec<_>>()
            .join(" ")
    };

    let dungeon_line = if is_create {
        format!(
            "🎁 On create, all **{dungeon_count}** built-in dungeons will be \
             attached. You can trim them later with `/tier remove-dungeon`."
        )
    } else {
        "Dungeons are managed via `/tier add-dungeon` and `/tier remove-dungeon`.".to_string()
    };

    let show_create_button = draft.runs_channel.is_none();
    let create_hint = if show_create_button {
        "\n\nDon't have a channel yet? Click **Create default channel** below and \
         I'll make a Raids category with a `{slug}-runs` text channel for you."
    } else {
        ""
    };

    let description = format!(
        "A **tier** is an isolated raid section (e.g. Main, Veterans, Elite).\n\
         Rename later with `/tier edit`.\n\
         \n\
         **Runs channel** — where headcount + run messages post\n{runs_display}\n\
         \n\
         **Access roles**\n{roles_display}\n\
         \n\
         {dungeon_line}{create_hint}"
    );

    let embed = CreateEmbed::new()
        .title(format!("🎯 First tier · {name}"))
        .description(description)
        .color(0x57F287);

    let runs_select = CreateSelectMenu::new(
        "setup:tier:runs",
        CreateSelectMenuKind::Channel {
            channel_types: Some(vec![ChannelType::Text]),
            default_channels: draft.runs_channel.map(|c| vec![c]),
        },
    )
    .placeholder("Runs channel (required)")
    .min_values(1)
    .max_values(1);

    let role_select = CreateSelectMenu::new(
        "setup:tier:roles",
        CreateSelectMenuKind::Role {
            default_roles: if draft.access_roles.is_empty() {
                None
            } else {
                Some(draft.access_roles.clone())
            },
        },
    )
    .placeholder("Access roles (optional — empty = everyone)")
    .min_values(0)
    .max_values(10);

    let save_label = if is_create {
        "Create tier"
    } else {
        "Save changes"
    };
    let mut buttons = vec![CreateButton::new("setup:tier:save")
        .label(save_label)
        .style(ButtonStyle::Success)
        .disabled(draft.runs_channel.is_none())];
    if show_create_button {
        buttons.push(
            CreateButton::new("setup:tier:create_channels")
                .label("Create default channel")
                .style(ButtonStyle::Primary),
        );
    }
    buttons.push(
        CreateButton::new("setup:tier:back")
            .label("← Back")
            .style(ButtonStyle::Secondary),
    );
    let actions = CreateActionRow::Buttons(buttons);

    (
        embed,
        vec![
            CreateActionRow::SelectMenu(runs_select),
            CreateActionRow::SelectMenu(role_select),
            actions,
        ],
    )
}

// ---------------------------------------------------------------------------
// Section: superadmin
// ---------------------------------------------------------------------------

async fn section_superadmin(
    ctx: BotContext<'_>,
    trigger: &ComponentInteraction,
) -> Result<(), BotError> {
    let guild_id = guild_id_i64(ctx);
    let (embed, components) = superadmin_view(ctx).await?;
    respond_with_view(ctx, trigger, embed, components).await?;

    let msg_id = trigger.message.id;
    loop {
        let Some(mci) = await_next(ctx, msg_id).await else {
            return Ok(());
        };

        match mci.data.custom_id.as_str() {
            "setup:superadmin:back" => {
                back_to_dashboard(ctx, &mci).await?;
                return Ok(());
            }
            "setup:superadmin:use_me" => {
                let uid = ctx.author().id.get() as i64;
                db::guild::set_superadmin(&ctx.data().db, guild_id, Some(uid)).await?;
                back_to_dashboard(ctx, &mci).await?;
                return Ok(());
            }
            "setup:superadmin:clear" => {
                db::guild::set_superadmin(&ctx.data().db, guild_id, None).await?;
                back_to_dashboard(ctx, &mci).await?;
                return Ok(());
            }
            "setup:superadmin:pick" => {
                if let ComponentInteractionDataKind::UserSelect { values } = &mci.data.kind {
                    let uid = values.first().map(|u| u.get() as i64);
                    db::guild::set_superadmin(&ctx.data().db, guild_id, uid).await?;
                }
                back_to_dashboard(ctx, &mci).await?;
                return Ok(());
            }
            _ => {
                mci.defer(ctx.http()).await?;
            }
        }
    }
}

async fn superadmin_view(ctx: BotContext<'_>) -> Result<(CreateEmbed, Vec<CreateActionRow>)> {
    let guild_id = guild_id_i64(ctx);
    let guild = db::guild::get(&ctx.data().db, guild_id)
        .await?
        .expect("guild row upserted by setup() entry, exists for the wizard's lifetime");

    let current = guild
        .superadmin_user_id
        .map(|uid| format!("Currently: <@{uid}>"))
        .unwrap_or_else(|| "Currently: _not set_".to_string());

    let embed = CreateEmbed::new()
        .title("👑 Superadmin bypass")
        .description(format!(
            "A superadmin bypasses **every** permission check in this server.\n\
             Use it as a safety net in case your permission rules lock you out.\n\
             Anyone with the Discord **Manage Server** permission already has full access, \
             so this is mostly a courtesy for a specific person.\n\
             \n\
             {current}"
        ))
        .color(0xFEE75C);

    let default_users = guild
        .superadmin_user_id
        .map(|uid| vec![UserId::new(uid as u64)]);

    let user_select = CreateSelectMenu::new(
        "setup:superadmin:pick",
        CreateSelectMenuKind::User { default_users },
    )
    .placeholder("Pick a user")
    .min_values(1)
    .max_values(1);

    let actions = CreateActionRow::Buttons(vec![
        CreateButton::new("setup:superadmin:use_me")
            .label("Use me")
            .style(ButtonStyle::Primary),
        CreateButton::new("setup:superadmin:clear")
            .label("Clear")
            .style(ButtonStyle::Danger)
            .disabled(guild.superadmin_user_id.is_none()),
        CreateButton::new("setup:superadmin:back")
            .label("← Back")
            .style(ButtonStyle::Secondary),
    ]);

    Ok((
        embed,
        vec![CreateActionRow::SelectMenu(user_select), actions],
    ))
}

// ---------------------------------------------------------------------------
// Section: log channel
// ---------------------------------------------------------------------------

async fn section_log_channel(
    ctx: BotContext<'_>,
    trigger: &ComponentInteraction,
) -> Result<(), BotError> {
    let guild_id = guild_id_i64(ctx);
    let (embed, components) = channel_section_view(
        ctx,
        "📜 Audit log channel",
        "Where Starship posts audit-worthy events (runs started, leaders changed, \
         permissions updated). Pick a channel only raid leaders and staff can see.\n\
         \n\
         Leave empty to disable audit logging.",
        |g| g.log_channel_id,
        "setup:log:pick",
        "setup:log:clear",
        "setup:log:back",
    )
    .await?;
    respond_with_view(ctx, trigger, embed, components).await?;

    let msg_id = trigger.message.id;
    loop {
        let Some(mci) = await_next(ctx, msg_id).await else {
            return Ok(());
        };
        match mci.data.custom_id.as_str() {
            "setup:log:back" => {
                back_to_dashboard(ctx, &mci).await?;
                return Ok(());
            }
            "setup:log:clear" => {
                db::guild::set_log_channel(&ctx.data().db, guild_id, None).await?;
                back_to_dashboard(ctx, &mci).await?;
                return Ok(());
            }
            "setup:log:pick" => {
                if let ComponentInteractionDataKind::ChannelSelect { values } = &mci.data.kind {
                    let cid = values.first().map(|c| c.get() as i64);
                    db::guild::set_log_channel(&ctx.data().db, guild_id, cid).await?;
                }
                back_to_dashboard(ctx, &mci).await?;
                return Ok(());
            }
            _ => {
                mci.defer(ctx.http()).await?;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Section: verification
// ---------------------------------------------------------------------------

async fn section_verification(
    ctx: BotContext<'_>,
    trigger: &ComponentInteraction,
) -> Result<(), BotError> {
    let guild_id = guild_id_i64(ctx);
    let pool = &ctx.data().db;

    let (embed, components) = verification_view(ctx).await?;
    respond_with_view(ctx, trigger, embed, components).await?;

    let msg_id = trigger.message.id;
    loop {
        let Some(mci) = await_next(ctx, msg_id).await else {
            return Ok(());
        };
        match mci.data.custom_id.as_str() {
            "setup:verify:back" => {
                back_to_dashboard(ctx, &mci).await?;
                return Ok(());
            }
            "setup:verify:role" => {
                if let ComponentInteractionDataKind::RoleSelect { values } = &mci.data.kind {
                    let rid = values.first().map(|r| r.get() as i64);
                    db::guild::set_verified_role(pool, guild_id, rid).await?;
                }
                let (embed, components) = verification_view(ctx).await?;
                respond_with_view(ctx, &mci, embed, components).await?;
            }
            "setup:verify:channel" => {
                if let ComponentInteractionDataKind::ChannelSelect { values } = &mci.data.kind {
                    let cid = values.first().map(|c| c.get() as i64);
                    db::guild::set_verify_channel(pool, guild_id, cid).await?;
                    // Picking a different channel invalidates any
                    // previously-posted message id — the old message
                    // lives in a different channel and would never be
                    // clicked. Clear it so the user has to repost.
                    db::guild::set_verify_message(pool, guild_id, None).await?;
                }
                let (embed, components) = verification_view(ctx).await?;
                respond_with_view(ctx, &mci, embed, components).await?;
            }
            "setup:verify:post" => match handle_verify_post(ctx, &mci).await {
                Ok(()) => {
                    let (embed, components) = verification_view(ctx).await?;
                    respond_with_view(ctx, &mci, embed, components).await?;
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "post verify message failed");
                    mci.create_response(
                        ctx.http(),
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .content(format!(
                                    "⚠ Couldn't post the Verify message: {e}\n\
                                     Make sure I have **Send Messages** in that channel."
                                ))
                                .ephemeral(true),
                        ),
                    )
                    .await?;
                }
            },
            "setup:verify:auto" => match handle_verify_auto(ctx, &mci).await {
                Ok(()) => {
                    let (embed, components) = verification_view(ctx).await?;
                    respond_with_view(ctx, &mci, embed, components).await?;
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "auto-provision verification failed");
                    mci.create_response(
                        ctx.http(),
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .content(format!(
                                    "⚠ Couldn't auto-provision verification: {e}\n\
                                     Make sure I have **Manage Channels** and \
                                     **Manage Roles**, then try again."
                                ))
                                .ephemeral(true),
                        ),
                    )
                    .await?;
                }
            },
            "setup:verify:clear" => {
                db::guild::set_verified_role(pool, guild_id, None).await?;
                db::guild::set_verify_channel(pool, guild_id, None).await?;
                db::guild::set_verify_message(pool, guild_id, None).await?;
                let (embed, components) = verification_view(ctx).await?;
                respond_with_view(ctx, &mci, embed, components).await?;
            }
            _ => {
                mci.defer(ctx.http()).await?;
            }
        }
    }
}

async fn verification_view(ctx: BotContext<'_>) -> Result<(CreateEmbed, Vec<CreateActionRow>)> {
    let guild_id = guild_id_i64(ctx);
    let guild = db::guild::get(&ctx.data().db, guild_id)
        .await?
        .expect("guild row upserted by setup() entry, exists for the wizard's lifetime");

    let role_line = guild
        .verified_role_id
        .map(|r| format!("Role: <@&{r}>"))
        .unwrap_or_else(|| "Role: _not set_".to_string());
    let channel_line = guild
        .verify_channel_id
        .map(|c| format!("Channel: <#{c}>"))
        .unwrap_or_else(|| "Channel: _not set_".to_string());
    let message_line = if guild.verify_message_id.is_some() {
        "Verify message: ✅ posted"
    } else {
        "Verify message: ⬜ not posted"
    };

    let body = format!(
        "Verification links Discord users to RealmEye in-game names. Once \
         configured, users can click the persistent **Verify** button (or run \
         `/verify`) to bind their account to their IGN.\n\
         \n\
         {role_line}\n\
         {channel_line}\n\
         {message_line}\n\
         \n\
         **Auto-provision** creates a `Verified` role + a `🔐verify` channel \
         and posts the button for you. Or pick an existing role / channel \
         and click **Post Verify message** to post manually."
    );

    let embed = CreateEmbed::new()
        .title("🔐 Verification")
        .description(body)
        .color(0x57F287);

    let role_select = CreateSelectMenu::new(
        "setup:verify:role",
        CreateSelectMenuKind::Role {
            default_roles: guild.verified_role_id.map(|r| vec![RoleId::new(r as u64)]),
        },
    )
    .placeholder("Verified role")
    .min_values(1)
    .max_values(1);

    let channel_select = CreateSelectMenu::new(
        "setup:verify:channel",
        CreateSelectMenuKind::Channel {
            channel_types: Some(vec![ChannelType::Text]),
            default_channels: guild
                .verify_channel_id
                .map(|c| vec![ChannelId::new(c as u64)]),
        },
    )
    .placeholder("Verify channel")
    .min_values(1)
    .max_values(1);

    let post_label = if guild.verify_message_id.is_some() {
        "Repost Verify message"
    } else {
        "Post Verify message"
    };
    let can_post = guild.verified_role_id.is_some() && guild.verify_channel_id.is_some();
    let any_set = guild.verified_role_id.is_some()
        || guild.verify_channel_id.is_some()
        || guild.verify_message_id.is_some();

    let actions = CreateActionRow::Buttons(vec![
        CreateButton::new("setup:verify:post")
            .label(post_label)
            .style(ButtonStyle::Success)
            .disabled(!can_post),
        CreateButton::new("setup:verify:auto")
            .label("Auto-provision")
            .style(ButtonStyle::Primary),
        CreateButton::new("setup:verify:clear")
            .label("Clear")
            .style(ButtonStyle::Danger)
            .disabled(!any_set),
        CreateButton::new("setup:verify:back")
            .label("← Back")
            .style(ButtonStyle::Secondary),
    ]);

    Ok((
        embed,
        vec![
            CreateActionRow::SelectMenu(role_select),
            CreateActionRow::SelectMenu(channel_select),
            actions,
        ],
    ))
}

/// Post (or repost) the persistent Verify-button message in the
/// configured channel. If a prior message is recorded and still alive,
/// it's deleted first so the new button is the only one. The new
/// message ID is persisted on the guild row.
async fn handle_verify_post(ctx: BotContext<'_>, _mci: &ComponentInteraction) -> Result<()> {
    let guild_id = guild_id_i64(ctx);
    let pool = &ctx.data().db;
    let guild = db::guild::get(pool, guild_id)
        .await?
        .expect("guild row upserted by setup() entry, exists for the wizard's lifetime");

    let Some(channel_raw) = guild.verify_channel_id else {
        anyhow::bail!("verify channel not set");
    };
    let channel_id = ChannelId::new(channel_raw as u64);
    let http = ctx.http();

    if let Some(prior) = guild.verify_message_id {
        let prior_id = MessageId::new(prior as u64);
        // Best-effort delete — 404 is fine, it means the message was
        // already gone. Permission-error stays in the log but doesn't
        // block posting the replacement.
        if let Err(e) = channel_id.delete_message(http, prior_id).await {
            tracing::warn!(error = ?e, "could not delete prior verify message");
        }
    }

    let new_id = find_or_post_verify_message(ctx, channel_id, None).await?;
    db::guild::set_verify_message(pool, guild_id, Some(new_id.get() as i64)).await?;
    Ok(())
}

/// Auto-provision: create role + channel + message in one click. Same
/// code path as quick-setup. Idempotent — re-running picks up existing
/// `Verified` / `🔐verify` artefacts rather than duplicating.
async fn handle_verify_auto(ctx: BotContext<'_>, mci: &ComponentInteraction) -> Result<()> {
    // Acknowledge silently so the channel-creation + message-post HTTP
    // sequence can take a moment without the button showing
    // "interaction failed". The caller refreshes the dashboard view
    // afterwards.
    mci.defer(ctx.http()).await?;

    let guild_id = guild_id_i64(ctx);
    let pool = &ctx.data().db;
    let prior = db::guild::get(pool, guild_id).await?;

    let role_id = find_or_create_verified_role(ctx).await?;
    let channel_id = find_or_create_verify_channel(ctx, role_id).await?;
    let prior_message = prior
        .as_ref()
        .and_then(|g| g.verify_message_id)
        .map(|m| MessageId::new(m as u64));
    let message_id = find_or_post_verify_message(ctx, channel_id, prior_message).await?;

    db::guild::set_verified_role(pool, guild_id, Some(role_id.get() as i64)).await?;
    db::guild::set_verify_channel(pool, guild_id, Some(channel_id.get() as i64)).await?;
    db::guild::set_verify_message(pool, guild_id, Some(message_id.get() as i64)).await?;
    Ok(())
}

/// Shared view builder for channel-picker sections.
async fn channel_section_view(
    ctx: BotContext<'_>,
    title: &str,
    body: &str,
    field: impl Fn(&crate::db::models::Guild) -> Option<i64>,
    pick_id: &str,
    clear_id: &str,
    back_id: &str,
) -> Result<(CreateEmbed, Vec<CreateActionRow>)> {
    let guild_id = guild_id_i64(ctx);
    let guild = db::guild::get(&ctx.data().db, guild_id)
        .await?
        .expect("guild row upserted by setup() entry, exists for the wizard's lifetime");
    let current_id = field(&guild);

    let current_display = current_id
        .map(|c| format!("Currently: <#{c}>"))
        .unwrap_or_else(|| "Currently: _not set_".to_string());

    let embed = CreateEmbed::new()
        .title(title)
        .description(format!("{body}\n\n{current_display}"))
        .color(0x5865F2);

    let pick = CreateSelectMenu::new(
        pick_id,
        CreateSelectMenuKind::Channel {
            channel_types: Some(vec![ChannelType::Text]),
            default_channels: current_id.map(|c| vec![ChannelId::new(c as u64)]),
        },
    )
    .placeholder("Pick a channel")
    .min_values(1)
    .max_values(1);

    let actions = CreateActionRow::Buttons(vec![
        CreateButton::new(clear_id)
            .label("Clear")
            .style(ButtonStyle::Danger)
            .disabled(current_id.is_none()),
        CreateButton::new(back_id)
            .label("← Back")
            .style(ButtonStyle::Secondary),
    ]);

    Ok((embed, vec![CreateActionRow::SelectMenu(pick), actions]))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn await_next(ctx: BotContext<'_>, msg_id: MessageId) -> Option<ComponentInteraction> {
    ComponentInteractionCollector::new(&ctx.serenity_context().shard)
        .message_id(msg_id)
        .author_id(ctx.author().id)
        .timeout(WIZARD_TIMEOUT)
        .await
}

async fn respond_with_view(
    ctx: BotContext<'_>,
    interaction: &ComponentInteraction,
    embed: CreateEmbed,
    components: Vec<CreateActionRow>,
) -> Result<(), BotError> {
    interaction
        .create_response(
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

async fn respond_plain(
    ctx: BotContext<'_>,
    interaction: &ComponentInteraction,
    content: &str,
) -> Result<(), BotError> {
    interaction
        .create_response(
            ctx.http(),
            CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new()
                    .content(content)
                    .embeds(vec![])
                    .components(vec![]),
            ),
        )
        .await?;
    Ok(())
}

async fn back_to_dashboard(
    ctx: BotContext<'_>,
    interaction: &ComponentInteraction,
) -> Result<(), BotError> {
    let (embed, components) = dashboard_view(ctx).await?;
    respond_with_view(ctx, interaction, embed, components).await
}

/// Find-or-create a "Raids" category + `{slug}-runs` text channel under it.
/// Idempotent — re-running picks up the existing channel rather than
/// duplicating. R3 collapsed the old headcount/raid split to a single
/// channel: both headcounts and runs post here.
async fn create_default_channels(ctx: BotContext<'_>, tier_name: &str) -> Result<ChannelId> {
    let guild_id = require_guild_id(ctx);
    let http = ctx.http();

    let slug = slugify(tier_name);
    let runs_name = format!("{slug}-runs");
    let category_name = "Raids";

    let existing = guild_id.channels(http).await?;

    let category_id = match existing
        .values()
        .find(|c| c.kind == ChannelType::Category && c.name.eq_ignore_ascii_case(category_name))
    {
        Some(c) => c.id,
        None => {
            guild_id
                .create_channel(
                    http,
                    CreateChannel::new(category_name).kind(ChannelType::Category),
                )
                .await?
                .id
        }
    };

    // Accept the new `{slug}-runs` name first; if this is an upgraded guild
    // whose pre-R3 channels still exist under their old names, pick the
    // raid-room channel as a courtesy so setup stays idempotent.
    let legacy_raid_name = format!("{slug}-raid-room");
    let runs_id = match existing.values().find(|c| {
        c.kind == ChannelType::Text
            && c.parent_id == Some(category_id)
            && (c.name.eq_ignore_ascii_case(&runs_name)
                || c.name.eq_ignore_ascii_case(&legacy_raid_name))
    }) {
        Some(c) => c.id,
        None => {
            guild_id
                .create_channel(
                    http,
                    CreateChannel::new(&runs_name)
                        .kind(ChannelType::Text)
                        .category(category_id),
                )
                .await?
                .id
        }
    };

    Ok(runs_id)
}

/// Discord channel names: lowercase, alphanumeric + hyphens, no runs of
/// punctuation. Discord will further sanitize on its end — this is best-effort.
fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut last_hyphen = true;
    for c in s.trim().chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_hyphen = false;
        } else if !last_hyphen {
            out.push('-');
            last_hyphen = true;
        }
    }
    out.trim_matches('-').to_string()
}
