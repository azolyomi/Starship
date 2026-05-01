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
use crate::services::{permission, start_run_ui_listing};
use crate::{db, guild_id_i64, require_guild_id, BotContext, BotError};

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
             • Creates `#🚀start-a-run` with a sticky **Start a run** \
               button so anyone can spin up a raid\n\
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
    // headcount / raid channels). Also returns the category so we can
    // place the start-run UI channel alongside.
    let (raids_category_id, runs_id) = create_default_channels(ctx, "Main").await?;

    // Log channel — emoji-prefixed name, fallback to plain if Discord
    // rejects the leading rocket glyph.
    let log_id = find_or_create_log_channel(ctx).await?;

    // Start-run UI channel — sticky button + active raids listing live
    // here. Lives under the Raids category alongside the runs channel.
    let so_channel_id = find_or_create_start_run_ui_channel(ctx, raids_category_id).await?;

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
            // Globals are implicitly visible to every tier — no bulk-attach
            // step needed. /tier add-dungeon now toggles per-tier disables
            // for globals and per-tier attachments for guild-specifics.
            created.id
        }
    };

    // Start-run UI: enable the tier with the freshly-created channel and
    // server defaults for idle / cooldown / min-reactors. Sticky messages
    // get installed below — best-effort so a transient Discord blip
    // doesn't fail the whole quick setup.
    db::tier::update_start_run_ui(
        pool,
        tier_id,
        Some(true),
        Some(so_channel_id.get() as i64),
        None,
        None,
        None,
    )
    .await?;

    db::guild::set_log_channel(pool, guild_id, Some(log_id.get() as i64)).await?;
    db::guild::set_superadmin(pool, guild_id, Some(user_id)).await?;

    // Raid Leader role + raid-management permission grants.
    let role_id = find_or_create_raid_leader_role(ctx).await?;
    for action in permission::LEADER_ACTIONS {
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

    // Install the start-run UI stickies after every other DB write has
    // landed. install_stickies_best_effort logs and continues on failure
    // — the operator can repost from /setup → Start-run UI → Repost
    // stickies if Discord refused the post.
    install_stickies_best_effort(ctx.serenity_context(), pool, tier_id).await;

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
             **Don't have a RealmEye password?**\n\
             In-game, message MrEyeball with `/tell MrEyeball password` and \
             they'll send you one. We're not affiliated with RealmEye and \
             don't control MrEyeball — **never share that password with us \
             or anyone else**, and we will never ask you for it.\n\
             \n\
             **Trouble?**\n\
             • Make sure your RealmEye **profile is set to public** \
                (RealmEye → Settings → Privacy) — descriptions can't be made \
                public on their own.\n\
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
            let so_chip = if t.enable_start_run_ui {
                " · start-run UI ✅"
            } else {
                ""
            };
            let extra = if tiers.len() > 1 {
                format!(
                    "\n_+ {} more tier(s) — manage with `/tier`._",
                    tiers.len() - 1
                )
            } else {
                String::new()
            };
            format!("**{}** — runs: {runs}{so_chip}{extra}", t.name)
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
         {tier_mark} **Tier** *(required)*\n\
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

    // Same label whether the tier exists or not — the section handles
    // create vs edit internally and "Setup tier" reads cleanly for both.
    let tier_label = "Setup tier";
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

    let dungeon_count = db::tier::list_visible_dungeons(&ctx.data().db, first.id, guild_id)
        .await?
        .len();

    let so_block = if first.enable_start_run_ui {
        match first.start_run_ui_channel_id {
            Some(c) => format!(
                "\nStart-run UI is **enabled** — anyone can start a raid \
                 from <#{c}>.\n",
            ),
            None => String::new(),
        }
    } else {
        String::new()
    };

    let description = format!(
        "Starship is ready to raid.\n\
         \n\
         **{name}** is live in {runs}.\n\
         {dungeon_count} dungeon(s) are attached to this tier.\n\
         {so_block}\
         \n\
         **Try it out**\n\
         • Click **Start a run** in the start-run UI channel (if enabled).\n\
         • Or run `/headcount <dungeon>` to start gathering raiders.\n\
         • Run `/pingroles` to subscribe to dungeon notifications.\n\
         \n\
         **Manage later**\n\
         • `/setup` → **Setup tier** → **Configure start-run UI** — \
           tune knobs, repost stickies, switch tiers\n\
         • `/tier` — add more tiers, change channels, assign access roles\n\
         • `/permission` — let specific roles run headcounts and runs\n\
         • `/dungeon` — customise or add dungeons\n\
         • `/pingroles set` — bind a dungeon to a notification role\n\
         • `/setup` — re-run this wizard any time",
        name = first.name,
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
    leader_roles: Vec<RoleId>,
    /// Roles granted *only* `StartHeadcount` for this tier. They can
    /// open headcounts via `/hc` or the start-run button but can't
    /// convert/cancel/manage other people's raids.
    member_roles: Vec<RoleId>,
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
            let leader = db::permission::list_leader_roles_for_tier(pool, t.id).await?;
            let member = db::permission::list_member_roles_for_tier(pool, t.id).await?;
            TierDraft {
                runs_channel: t.runs_channel_id.map(|id| ChannelId::new(id as u64)),
                leader_roles: leader.into_iter().map(|r| RoleId::new(r as u64)).collect(),
                member_roles: member.into_iter().map(|r| RoleId::new(r as u64)).collect(),
            }
        }
        None => TierDraft {
            runs_channel: None,
            leader_roles: Vec::new(),
            member_roles: Vec::new(),
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
                    draft.leader_roles = values.clone();
                }
                let (embed, components) =
                    tier_view(existing.as_ref(), &draft, global_dungeons.len());
                respond_with_view(ctx, &mci, embed, components).await?;
            }
            "setup:tier:member_roles" => {
                if let ComponentInteractionDataKind::RoleSelect { values } = &mci.data.kind {
                    draft.member_roles = values.clone();
                }
                let (embed, components) =
                    tier_view(existing.as_ref(), &draft, global_dungeons.len());
                respond_with_view(ctx, &mci, embed, components).await?;
            }
            "setup:tier:create_channels" => {
                let tier_name = existing.as_ref().map(|t| t.name.as_str()).unwrap_or("Main");
                match create_default_channels(ctx, tier_name).await {
                    Ok((_, runs_id)) => {
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
            "setup:tier:so" => {
                // Hand off to the start-run UI sub-view (it has its own
                // click loop and "Back" returns to the dashboard). Only
                // reachable when the tier exists in the DB — the button
                // is disabled before first save.
                //
                // We RETURN here rather than fall through so the outer
                // dashboard loop resumes awaiting clicks; otherwise this
                // tier-section loop would race the dashboard loop on the
                // same message.
                section_start_run_ui(ctx, &mci).await?;
                return Ok(());
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
                        // Globals are implicitly visible to every tier —
                        // no bulk-attach loop needed.
                        created.id
                    }
                };

                sync_leader_roles(pool, guild_id, tier_id, &draft.leader_roles).await?;
                sync_member_roles(pool, guild_id, tier_id, &draft.member_roles).await?;

                back_to_dashboard(ctx, &mci).await?;
                return Ok(());
            }
            _ => {
                mci.defer(ctx.http()).await?;
            }
        }
    }
}

/// Reconcile the set of "leader" roles for a tier: each desired role gets the
/// full `LEADER_ACTIONS` set scoped to this tier; any roles previously granted
/// at this scope but absent from `desired` get all those actions revoked.
async fn sync_leader_roles(
    pool: &sqlx::PgPool,
    guild_id: i64,
    tier_id: i32,
    desired: &[RoleId],
) -> Result<()> {
    let existing: std::collections::HashSet<i64> =
        db::permission::list_leader_roles_for_tier(pool, tier_id)
            .await?
            .into_iter()
            .collect();
    let desired_set: std::collections::HashSet<i64> =
        desired.iter().map(|r| r.get() as i64).collect();

    for add in desired_set.difference(&existing) {
        for action in permission::LEADER_ACTIONS {
            db::permission::grant(pool, guild_id, *add, action, Some(tier_id), None).await?;
        }
    }
    for remove in existing.difference(&desired_set) {
        for action in permission::LEADER_ACTIONS {
            db::permission::revoke(pool, guild_id, *remove, action, Some(tier_id), None).await?;
        }
    }
    Ok(())
}

/// Reconcile the set of member-tier roles for a tier: each desired role gets
/// just `StartHeadcount` scoped to this tier (no convert/cancel/manage), and
/// any role previously in the member list but absent from `desired` has that
/// grant revoked. Roles also present in the leader list pass through
/// untouched: `list_member_roles_for_tier` filters them out, so they never
/// appear in `existing` here.
async fn sync_member_roles(
    pool: &sqlx::PgPool,
    guild_id: i64,
    tier_id: i32,
    desired: &[RoleId],
) -> Result<()> {
    let existing: std::collections::HashSet<i64> =
        db::permission::list_member_roles_for_tier(pool, tier_id)
            .await?
            .into_iter()
            .collect();
    let desired_set: std::collections::HashSet<i64> =
        desired.iter().map(|r| r.get() as i64).collect();

    for add in desired_set.difference(&existing) {
        db::permission::grant(pool, guild_id, *add, "StartHeadcount", Some(tier_id), None).await?;
    }
    for remove in existing.difference(&desired_set) {
        db::permission::revoke(
            pool,
            guild_id,
            *remove,
            "StartHeadcount",
            Some(tier_id),
            None,
        )
        .await?;
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
    let leader_display = if draft.leader_roles.is_empty() {
        "_none — only Discord admins / the bot superadmin can lead raids in this tier_".to_string()
    } else {
        draft
            .leader_roles
            .iter()
            .map(|r| format!("<@&{r}>"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let member_display = if draft.member_roles.is_empty() {
        "_none — leaders and admins can already start headcounts via `/hc`_".to_string()
    } else {
        draft
            .member_roles
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
         **Leader roles** — full raid management (start, convert, cancel, end)\n{leader_display}\n\
         \n\
         **Headcount roles** — can start a headcount (`/hc` or sticky button) but \
         can't manage other people's raids\n{member_display}\n\
         \n\
         {dungeon_line}{create_hint}"
    );

    let embed = CreateEmbed::new()
        .title(format!("🎯 Setup tier · {name}"))
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

    let leader_select = CreateSelectMenu::new(
        "setup:tier:roles",
        CreateSelectMenuKind::Role {
            default_roles: if draft.leader_roles.is_empty() {
                None
            } else {
                Some(draft.leader_roles.clone())
            },
        },
    )
    .placeholder("Leader roles (full raid management)")
    .min_values(0)
    .max_values(10);

    let member_select = CreateSelectMenu::new(
        "setup:tier:member_roles",
        CreateSelectMenuKind::Role {
            default_roles: if draft.member_roles.is_empty() {
                None
            } else {
                Some(draft.member_roles.clone())
            },
        },
    )
    .placeholder("Headcount roles (can start, can't manage others)")
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
    // Configure start-run UI: only meaningful once the tier exists in
    // the DB (the SO knobs live on the tier row). Shown disabled before
    // first save so the user knows where to find SO config without
    // needing to discover it elsewhere.
    let so_enabled = existing.map(|t| t.enable_start_run_ui).unwrap_or(false);
    let so_label = if so_enabled {
        "Configure start-run UI ✅"
    } else {
        "Configure start-run UI"
    };
    buttons.push(
        CreateButton::new("setup:tier:so")
            .label(so_label)
            .style(ButtonStyle::Secondary)
            .disabled(existing.is_none()),
    );
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
            CreateActionRow::SelectMenu(leader_select),
            CreateActionRow::SelectMenu(member_select),
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

// ---------------------------------------------------------------------------
// Section: start-run UI
// ---------------------------------------------------------------------------

/// Idle / cooldown / min-reactor presets shown in the section's StringSelects.
/// First column is the value persisted; second is the user-facing label
/// suffix. The full label rendered to Discord is "{prefix}: {suffix}" so
/// the closed dropdown still tells the user what it controls once a value
/// is selected (Discord replaces the placeholder with the chosen option's
/// label).
const SO_IDLE_PRESETS: &[(i32, &str)] = &[
    (5, "5 minutes"),
    (10, "10 minutes"),
    (15, "15 minutes"),
    (30, "30 minutes"),
    (60, "60 minutes"),
];
const SO_COOLDOWN_PRESETS: &[(i32, &str)] = &[
    (60, "1 minute"),
    (180, "3 minutes"),
    (300, "5 minutes"),
    (600, "10 minutes"),
    (1800, "30 minutes"),
];
const SO_MIN_REACTORS_PRESETS: &[(i32, &str)] = &[
    (1, "1"),
    (2, "2"),
    (3, "3"),
    (4, "4"),
    (5, "5"),
    (6, "6"),
    (8, "8"),
];

/// Which sub-view of the start-run UI section is currently rendered.
/// Multi-tier guilds enter the section on `Picker`; single-tier guilds
/// skip straight to `Config`.
#[derive(Clone, Copy)]
enum SoView {
    Picker,
    Config,
}

async fn section_start_run_ui(
    ctx: BotContext<'_>,
    trigger: &ComponentInteraction,
) -> Result<(), BotError> {
    let guild_id = guild_id_i64(ctx);
    let pool = &ctx.data().db;

    let tiers = db::tier::list(pool, guild_id).await?;
    // Pre-select an enabled tier when one exists so re-entering the section
    // lands on the in-use config rather than a freshly-picked default.
    let Some(initial_tier) = tiers
        .iter()
        .find(|t| t.enable_start_run_ui)
        .or_else(|| tiers.first())
        .cloned()
    else {
        respond_plain(
            ctx,
            trigger,
            "Set up the first tier before configuring start-run UI.",
        )
        .await?;
        return Ok(());
    };

    let mut current_tier_id = initial_tier.id;
    let mut view = if tiers.len() == 1 {
        SoView::Config
    } else {
        SoView::Picker
    };

    let (embed, components) = render_start_run_ui_view(&tiers, &initial_tier, view);
    respond_with_view(ctx, trigger, embed, components).await?;

    let msg_id = trigger.message.id;
    let serenity_ctx = ctx.serenity_context();
    loop {
        let Some(mci) = await_next(ctx, msg_id).await else {
            return Ok(());
        };

        // Re-load tiers each iteration so a /tier create or /tier delete in
        // another tab during the wizard flow is reflected immediately.
        let tiers = db::tier::list(pool, guild_id).await?;
        let Some(tier) = tiers.iter().find(|t| t.id == current_tier_id).cloned() else {
            respond_plain(
                ctx,
                &mci,
                "The selected tier was deleted. Run `/setup` again to start over.",
            )
            .await?;
            return Ok(());
        };

        match mci.data.custom_id.as_str() {
            "setup:so:back" => {
                back_to_dashboard(ctx, &mci).await?;
                return Ok(());
            }
            "setup:so:switch" => {
                view = SoView::Picker;
                let (embed, components) = render_start_run_ui_view(&tiers, &tier, view);
                respond_with_view(ctx, &mci, embed, components).await?;
            }
            "setup:so:tierpick" => {
                if let Some(new_id) = parse_select_i32(&mci.data.kind) {
                    current_tier_id = new_id;
                }
                view = SoView::Config;
                let new_tier = current_tier(pool, current_tier_id)
                    .await?
                    .unwrap_or(tier.clone());
                let (embed, components) = render_start_run_ui_view(&tiers, &new_tier, view);
                respond_with_view(ctx, &mci, embed, components).await?;
            }
            "setup:so:channel" => {
                if let ComponentInteractionDataKind::ChannelSelect { values } = &mci.data.kind {
                    let new_channel: Option<i64> = values.first().map(|c| c.get() as i64);
                    let prior_channel = tier.start_run_ui_channel_id;
                    // Channel changed: tear down the stickies in the old
                    // channel before clearing IDs. teardown_messages handles
                    // the delete + null-out atomically per row.
                    if new_channel != prior_channel
                        && (tier.start_run_ui_button_message_id.is_some()
                            || tier.start_run_ui_listing_message_id.is_some())
                    {
                        if let Err(e) =
                            start_run_ui_listing::teardown_messages(serenity_ctx, pool, &tier).await
                        {
                            tracing::warn!(
                                error = ?e,
                                tier_id = tier.id,
                                "failed to clean up stickies on channel change",
                            );
                        }
                    }
                    db::tier::update_start_run_ui(
                        pool,
                        tier.id,
                        None,
                        new_channel,
                        None,
                        None,
                        None,
                    )
                    .await?;
                }
                let refreshed = current_tier(pool, tier.id).await?.unwrap_or(tier.clone());
                let (embed, components) = render_start_run_ui_view(&tiers, &refreshed, view);
                respond_with_view(ctx, &mci, embed, components).await?;
            }
            "setup:so:idle" => {
                let value = parse_select_i32(&mci.data.kind);
                if value.is_some() {
                    db::tier::update_start_run_ui(pool, tier.id, None, None, value, None, None)
                        .await?;
                }
                let refreshed = current_tier(pool, tier.id).await?.unwrap_or(tier.clone());
                let (embed, components) = render_start_run_ui_view(&tiers, &refreshed, view);
                respond_with_view(ctx, &mci, embed, components).await?;
            }
            "setup:so:cooldown" => {
                let value = parse_select_i32(&mci.data.kind);
                if value.is_some() {
                    db::tier::update_start_run_ui(pool, tier.id, None, None, None, value, None)
                        .await?;
                }
                let refreshed = current_tier(pool, tier.id).await?.unwrap_or(tier.clone());
                let (embed, components) = render_start_run_ui_view(&tiers, &refreshed, view);
                respond_with_view(ctx, &mci, embed, components).await?;
            }
            "setup:so:min" => {
                let value = parse_select_i32(&mci.data.kind);
                if value.is_some() {
                    db::tier::update_start_run_ui(pool, tier.id, None, None, None, None, value)
                        .await?;
                }
                let refreshed = current_tier(pool, tier.id).await?.unwrap_or(tier.clone());
                let (embed, components) = render_start_run_ui_view(&tiers, &refreshed, view);
                respond_with_view(ctx, &mci, embed, components).await?;
            }
            "setup:so:toggle" => {
                let live = current_tier(pool, tier.id).await?.unwrap_or(tier.clone());
                let want_enable = !live.enable_start_run_ui;

                if want_enable && live.start_run_ui_channel_id.is_none() {
                    mci.create_response(
                        ctx.http(),
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .content("Pick a channel before enabling start-run UI.")
                                .ephemeral(true),
                        ),
                    )
                    .await?;
                    continue;
                }

                if !want_enable {
                    // Tear down stickies before flipping the flag so a
                    // racing click in the small window between flag-flip and
                    // delete still hits a sticky owned by an enabled tier.
                    if let Err(e) =
                        start_run_ui_listing::teardown_messages(serenity_ctx, pool, &live).await
                    {
                        tracing::warn!(
                            error = ?e,
                            tier_id = tier.id,
                            "failed to tear down stickies on disable",
                        );
                    }
                }

                db::tier::update_start_run_ui(
                    pool,
                    tier.id,
                    Some(want_enable),
                    None,
                    None,
                    None,
                    None,
                )
                .await?;

                if want_enable {
                    install_stickies_best_effort(serenity_ctx, pool, tier.id).await;
                }

                let refreshed = current_tier(pool, tier.id).await?.unwrap_or(tier.clone());
                let (embed, components) = render_start_run_ui_view(&tiers, &refreshed, view);
                respond_with_view(ctx, &mci, embed, components).await?;
            }
            "setup:so:repost" => {
                let live = current_tier(pool, tier.id).await?.unwrap_or(tier.clone());
                if !live.enable_start_run_ui {
                    mci.create_response(
                        ctx.http(),
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .content("Start-run UI is disabled — enable it to post stickies.")
                                .ephemeral(true),
                        ),
                    )
                    .await?;
                    continue;
                }
                if live.start_run_ui_channel_id.is_none() {
                    mci.create_response(
                        ctx.http(),
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .content("Pick a channel first.")
                                .ephemeral(true),
                        ),
                    )
                    .await?;
                    continue;
                }
                // Tear down + reinstall: covers the case where the operator
                // manually deleted one or both stickies and wants fresh ones
                // without restarting the bot.
                if let Err(e) =
                    start_run_ui_listing::teardown_messages(serenity_ctx, pool, &live).await
                {
                    tracing::warn!(
                        error = ?e,
                        tier_id = tier.id,
                        "failed to clean up stickies before repost",
                    );
                }
                install_stickies_best_effort(serenity_ctx, pool, tier.id).await;

                let refreshed = current_tier(pool, tier.id).await?.unwrap_or(tier.clone());
                let (embed, components) = render_start_run_ui_view(&tiers, &refreshed, view);
                respond_with_view(ctx, &mci, embed, components).await?;
            }
            _ => {
                mci.defer(ctx.http()).await?;
            }
        }
    }
}

/// Install both sticky messages, best-effort. Runs ensure_button then
/// re-loads the tier so ensure_listing sees the freshly-stored
/// button_message_id (and similarly across the listing call). Failures
/// are logged but never bubbled — the wizard view will still re-render.
async fn install_stickies_best_effort(
    serenity_ctx: &serenity::Context,
    pool: &sqlx::PgPool,
    tier_id: i32,
) {
    let Some(tier) = current_tier(pool, tier_id).await.ok().flatten() else {
        return;
    };
    if let Err(e) = start_run_ui_listing::ensure_button_message(serenity_ctx, pool, &tier).await {
        tracing::warn!(
            error = ?e,
            tier_id,
            "failed to install start-run UI button message",
        );
    }
    let Some(after_button) = current_tier(pool, tier_id).await.ok().flatten() else {
        return;
    };
    if let Err(e) =
        start_run_ui_listing::ensure_listing_message(serenity_ctx, pool, &after_button).await
    {
        tracing::warn!(
            error = ?e,
            tier_id,
            "failed to install start-run UI listing message",
        );
    }
}

async fn current_tier(pool: &sqlx::PgPool, tier_id: i32) -> Result<Option<Tier>> {
    db::tier::get_by_id(pool, tier_id).await
}

fn parse_select_i32(kind: &ComponentInteractionDataKind) -> Option<i32> {
    if let ComponentInteractionDataKind::StringSelect { values } = kind {
        values.first().and_then(|v| v.parse().ok())
    } else {
        None
    }
}

fn so_preset_options(
    presets: &[(i32, &str)],
    current: i32,
    prefix: &str,
) -> Vec<serenity::CreateSelectMenuOption> {
    presets
        .iter()
        .map(|(value, label)| {
            let full = format!("{prefix}: {label}");
            let mut opt = serenity::CreateSelectMenuOption::new(full, value.to_string());
            if *value == current {
                opt = opt.default_selection(true);
            }
            opt
        })
        .collect()
}

fn render_start_run_ui_view(
    tiers: &[Tier],
    current: &Tier,
    view: SoView,
) -> (CreateEmbed, Vec<CreateActionRow>) {
    match view {
        SoView::Picker => so_picker_view(tiers, current),
        SoView::Config => so_config_view(tiers, current),
    }
}

fn so_picker_view(tiers: &[Tier], current: &Tier) -> (CreateEmbed, Vec<CreateActionRow>) {
    let mut summary = String::with_capacity(64 * tiers.len());
    for t in tiers {
        let mark = if t.enable_start_run_ui { "✅" } else { "⬜" };
        let chan = t
            .start_run_ui_channel_id
            .map(|c| format!(" <#{c}>"))
            .unwrap_or_default();
        summary.push_str(&format!("{mark} **{}**{chan}\n", t.name));
    }

    let body = format!(
        "Start-run UI is configured per tier. Pick the tier you want to \
         configure below.\n\n{summary}",
    );

    let embed = CreateEmbed::new()
        .title("\u{1F680} Start-run UI \u{2014} pick a tier")
        .description(body)
        .color(0x5865F2);

    let options: Vec<serenity::CreateSelectMenuOption> = tiers
        .iter()
        .map(|t| {
            let label = if t.enable_start_run_ui {
                format!("{} (enabled)", t.name)
            } else {
                t.name.clone()
            };
            let mut opt = serenity::CreateSelectMenuOption::new(label, t.id.to_string());
            if t.id == current.id {
                opt = opt.default_selection(true);
            }
            opt
        })
        .collect();

    let picker = CreateSelectMenu::new(
        "setup:so:tierpick",
        CreateSelectMenuKind::String { options },
    )
    .placeholder("Tier to configure")
    .min_values(1)
    .max_values(1);

    let actions = CreateActionRow::Buttons(vec![CreateButton::new("setup:so:back")
        .label("\u{2190} Back")
        .style(ButtonStyle::Secondary)]);

    (embed, vec![CreateActionRow::SelectMenu(picker), actions])
}

fn so_config_view(tiers: &[Tier], tier: &Tier) -> (CreateEmbed, Vec<CreateActionRow>) {
    let enabled = tier.enable_start_run_ui;
    let channel_line = tier
        .start_run_ui_channel_id
        .map(|c| format!("<#{c}>"))
        .unwrap_or_else(|| "_not set_".to_string());

    let runs_line = tier
        .runs_channel_id
        .map(|c| format!("<#{c}>"))
        .unwrap_or_else(|| "_not set — set in `Setup tier` or `/tier edit`_".to_string());

    let body = format!(
        "Per-tier opt-in: any user can start a headcount via a sticky **Start a run** \
         button — no leader role required.\n\
         \n\
         Anti-troll guardrails (configured below):\n\
         • One raid per (tier, dungeon) at a time\n\
         • One start-run UId raid per leader at a time\n\
         • Idle headcounts auto-cancel after the configured window\n\
         • Cancel cooldown after a leader self-cancels\n\
         • Minimum reactors required to convert HC \u{2192} Run\n\
         \n\
         **Tier:** {tier_name}\n\
         **Status:** {status}\n\
         **Sticky channel:** {channel_line}\n\
         **Headcounts post to:** {runs_line}\n\
         **Idle window:** {idle} minutes\n\
         **Cancel cooldown:** {cd} seconds\n\
         **Min reactors:** {min}\n\
         \n\
         _Tip: lock the sticky channel down to read-only for everyone but \
         the bot to keep the sticky messages near the top._",
        tier_name = tier.name,
        status = if enabled {
            "✅ enabled"
        } else {
            "⬜ disabled"
        },
        idle = tier.hc_idle_minutes,
        cd = tier.hc_cancel_cooldown_seconds,
        min = tier.hc_min_reactors,
    );

    let embed = CreateEmbed::new()
        .title(format!("\u{1F680} Start-run UI \u{2014} {}", tier.name))
        .description(body)
        .color(0x5865F2);

    let channel_select = CreateSelectMenu::new(
        "setup:so:channel",
        CreateSelectMenuKind::Channel {
            channel_types: Some(vec![ChannelType::Text]),
            default_channels: tier
                .start_run_ui_channel_id
                .map(|c| vec![ChannelId::new(c as u64)]),
        },
    )
    .placeholder("Sticky channel (button + listing live here)")
    .min_values(0)
    .max_values(1);

    let idle_select = CreateSelectMenu::new(
        "setup:so:idle",
        CreateSelectMenuKind::String {
            options: so_preset_options(SO_IDLE_PRESETS, tier.hc_idle_minutes, "Idle"),
        },
    )
    .placeholder("Idle window before HCs auto-cancel")
    .min_values(0)
    .max_values(1);

    let cooldown_select = CreateSelectMenu::new(
        "setup:so:cooldown",
        CreateSelectMenuKind::String {
            options: so_preset_options(
                SO_COOLDOWN_PRESETS,
                tier.hc_cancel_cooldown_seconds,
                "Cooldown",
            ),
        },
    )
    .placeholder("Cooldown after a leader self-cancels")
    .min_values(0)
    .max_values(1);

    let min_select = CreateSelectMenu::new(
        "setup:so:min",
        CreateSelectMenuKind::String {
            options: so_preset_options(
                SO_MIN_REACTORS_PRESETS,
                tier.hc_min_reactors,
                "Min reactors",
            ),
        },
    )
    .placeholder("Minimum reactors to convert HC \u{2192} Run")
    .min_values(0)
    .max_values(1);

    let toggle_label = if enabled { "Disable" } else { "Enable" };
    let toggle_style = if enabled {
        ButtonStyle::Danger
    } else {
        ButtonStyle::Success
    };

    let mut buttons = vec![CreateButton::new("setup:so:toggle")
        .label(toggle_label)
        .style(toggle_style)];
    if enabled {
        buttons.push(
            CreateButton::new("setup:so:repost")
                .label("Repost stickies")
                .style(ButtonStyle::Secondary),
        );
    }
    if tiers.len() > 1 {
        buttons.push(
            CreateButton::new("setup:so:switch")
                .label("Switch tier")
                .style(ButtonStyle::Secondary),
        );
    }
    buttons.push(
        CreateButton::new("setup:so:back")
            .label("\u{2190} Back")
            .style(ButtonStyle::Secondary),
    );

    (
        embed,
        vec![
            CreateActionRow::SelectMenu(channel_select),
            CreateActionRow::SelectMenu(idle_select),
            CreateActionRow::SelectMenu(cooldown_select),
            CreateActionRow::SelectMenu(min_select),
            CreateActionRow::Buttons(buttons),
        ],
    )
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

/// Find-or-create a "Raids" category + `{slug}-runs` text channel under
/// it. Returns `(category_id, runs_id)` so the caller can provision
/// further channels (e.g. start-run UI) under the same category.
/// Idempotent — re-running picks up the existing channel rather than
/// duplicating. R3 collapsed the old headcount/raid split to a single
/// channel: both headcounts and runs post here.
async fn create_default_channels(
    ctx: BotContext<'_>,
    tier_name: &str,
) -> Result<(ChannelId, ChannelId)> {
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

    Ok((category_id, runs_id))
}

/// Find-or-create a `🚀start-a-run` text channel under the Raids
/// category. Falls back to a plain `start-run UI` name if Discord
/// rejects the leading rocket glyph (some guild settings choke on
/// leading emoji). Idempotent.
async fn find_or_create_start_run_ui_channel(
    ctx: BotContext<'_>,
    category_id: ChannelId,
) -> Result<ChannelId> {
    const FANCY: &str = "🚀start-a-run";
    const PLAIN: &str = "start-a-run";

    let guild_id = require_guild_id(ctx);
    let http = ctx.http();
    let existing = guild_id.channels(http).await?;

    for name in [FANCY, PLAIN] {
        if let Some(c) = existing.values().find(|c| {
            c.kind == ChannelType::Text
                && c.parent_id == Some(category_id)
                && c.name.eq_ignore_ascii_case(name)
        }) {
            return Ok(c.id);
        }
    }

    let create = CreateChannel::new(FANCY)
        .kind(ChannelType::Text)
        .category(category_id);
    match guild_id.create_channel(http, create).await {
        Ok(ch) => Ok(ch.id),
        Err(e) => {
            tracing::warn!(
                error = ?e,
                "start-a-run channel with emoji prefix rejected, falling back",
            );
            Ok(guild_id
                .create_channel(
                    http,
                    CreateChannel::new(PLAIN)
                        .kind(ChannelType::Text)
                        .category(category_id),
                )
                .await?
                .id)
        }
    }
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
