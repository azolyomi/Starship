use anyhow::{bail, Result};
use poise::serenity_prelude as serenity;
use sqlx::PgPool;

use crate::{db, BotContext};

/// Hardcoded operator user ID. Bypasses every permission check in every guild,
/// including the Discord "Manage Server" gate on `/setup`. This is the
/// dead-man's-switch in case the per-guild permission rules ever lock the
/// operator out.
pub const GLOBAL_SUPERADMIN_USER_ID: u64 = 942_320_785_287_184_464;

fn is_global_superadmin(ctx: BotContext<'_>) -> bool {
    ctx.author().id.get() == GLOBAL_SUPERADMIN_USER_ID
}

/// Authoritative permission action set.
///
/// Mirrors `ALL_ACTIONS` below — that constant is what `/permission grant`
/// autocompletes against and what `db::permission::check` matches. Every
/// variant is part of the public permission contract even when no command
/// calls `require(Action::X, …)` directly today (e.g. `ManageRuns` is
/// checked via `can_organize`'s string path, `EndRun` is leader-gated
/// rather than action-gated). Keep enum, `as_str`, and `ALL_ACTIONS` in
/// lockstep.
#[allow(dead_code)] // see doc comment — variants are the authoritative registry
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    StartHeadcount,
    ConvertHeadcount,
    CancelHeadcount,
    StartRun,
    EndRun,
    ManageRuns,
    CreateVcRaid,
    ConfigureGuild,
    ManageTiers,
    ManagePermissions,
    ManageDungeons,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::StartHeadcount => "StartHeadcount",
            Action::ConvertHeadcount => "ConvertHeadcount",
            Action::CancelHeadcount => "CancelHeadcount",
            Action::StartRun => "StartRun",
            Action::EndRun => "EndRun",
            Action::ManageRuns => "ManageRuns",
            Action::CreateVcRaid => "CreateVcRaid",
            Action::ConfigureGuild => "ConfigureGuild",
            Action::ManageTiers => "ManageTiers",
            Action::ManagePermissions => "ManagePermissions",
            Action::ManageDungeons => "ManageDungeons",
        }
    }
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Requires the caller to have Discord "Manage Server" permission.
/// Used for commands that must be accessible before the bot's own permission
/// system is configured (e.g. `/setup`).
pub async fn require_discord_admin(ctx: BotContext<'_>) -> Result<()> {
    if is_global_superadmin(ctx) {
        return Ok(());
    }

    let member = ctx
        .author_member()
        .await
        .ok_or_else(|| anyhow::anyhow!("This command can only be used in a server."))?;

    let ok = member
        .permissions
        .map(|p| p.manage_guild() || p.administrator())
        .unwrap_or(false);

    if !ok {
        bail!("You need the **Manage Server** permission to run this command.");
    }
    Ok(())
}

/// Requires the caller to have `action` in this guild.
///
/// Bypass order:
/// 1. Superadmin (guild.superadmin_user_id)
/// 2. Role-based grant in the permissions table
///
/// `tier_id` / `dungeon_template_id` narrow the scope; pass `None` for
/// guild-wide checks (e.g. ManageTiers doesn't care about specific dungeons).
pub async fn require(
    ctx: BotContext<'_>,
    action: Action,
    tier_id: Option<i32>,
    dungeon_template_id: Option<i32>,
) -> Result<()> {
    if is_global_superadmin(ctx) {
        return Ok(());
    }

    let guild_id = ctx
        .guild_id()
        .ok_or_else(|| anyhow::anyhow!("This command can only be used in a server."))?
        .get() as i64;

    let caller_id = ctx.author().id.get() as i64;

    // Superadmin bypass. (A framework command_check already ensures the
    // guild row exists before any non-`/setup` command runs.)
    if let Some(guild) = db::guild::get(&ctx.data().db, guild_id).await? {
        if guild.superadmin_user_id == Some(caller_id) {
            return Ok(());
        }
    }

    // Discord "Manage Server" bypass — server admins always have full access.
    if let Some(member) = ctx.author_member().await {
        if member
            .permissions
            .map(|p| p.manage_guild() || p.administrator())
            .unwrap_or(false)
        {
            return Ok(());
        }
    }

    // Role-based check.
    let role_ids: Vec<i64> = ctx
        .author_member()
        .await
        .map(|m| m.roles.iter().map(|r| r.get() as i64).collect())
        .unwrap_or_default();

    let allowed = db::permission::check(
        &ctx.data().db,
        guild_id,
        &role_ids,
        action.as_str(),
        tier_id,
        dungeon_template_id,
    )
    .await?;

    if !allowed {
        bail!("You don't have permission to perform `{action}`.");
    }
    Ok(())
}

/// Action set granted to "raid leader" roles — express setup grants these
/// guild-wide to a Raid Leader role; the custom-setup wizard grants them
/// scoped to a specific tier. Keep ordering stable: callers iterate this
/// to issue grants/revokes.
pub const LEADER_ACTIONS: &[&str] = &[
    "StartHeadcount",
    "ConvertHeadcount",
    "CancelHeadcount",
    "StartRun",
    "EndRun",
    "ManageRuns",
    "CreateVcRaid",
];

/// All valid action names, for use in autocomplete and validation.
pub const ALL_ACTIONS: &[&str] = &[
    "StartHeadcount",
    "ConvertHeadcount",
    "CancelHeadcount",
    "StartRun",
    "EndRun",
    "ManageRuns",
    "CreateVcRaid",
    "ConfigureGuild",
    "ManageTiers",
    "ManagePermissions",
    "ManageDungeons",
];

pub fn is_valid_action(s: &str) -> bool {
    ALL_ACTIONS.contains(&s)
}

// ---------------------------------------------------------------------------
// Component-handler helpers
//
// These take raw `(pool, caller_id, role_ids, ...)` because component /
// modal handlers don't have a `BotContext` — they run off `serenity::Context`
// + the `BotData`. Mirrors the superadmin / Discord-admin bypass chain from
// `require` so the gating rules stay uniform.
// ---------------------------------------------------------------------------

/// Organizer gate for run / headcount lifecycle buttons (Start, Cancel, End,
/// Control Panel, etc.). Returns true when any of these hold:
///   1. caller is the global operator
///   2. caller is the guild's configured superadmin
///   3. caller has Discord "Manage Server" / "Administrator" (inferred from
///      the Discord permissions bitset the gateway hands us)
///   4. caller IS the raid leader
///   5. caller has the `ManageRuns` action granted, scoped to this
///      (tier, dungeon) or broader
// Each parameter is genuinely orthogonal — caller identity, caller's
// Discord-side permissions, the leader, and the scope tuple. Naming wins
// over a struct here; keeping the explicit signature.
#[allow(clippy::too_many_arguments)]
pub async fn can_organize(
    pool: &PgPool,
    guild_id: i64,
    caller_id: i64,
    caller_perms: Option<serenity::Permissions>,
    caller_role_ids: &[i64],
    leader_user_id: i64,
    tier_id: Option<i32>,
    dungeon_template_id: Option<i32>,
) -> Result<bool> {
    if caller_id == GLOBAL_SUPERADMIN_USER_ID as i64 {
        return Ok(true);
    }
    if caller_id == leader_user_id {
        return Ok(true);
    }
    if let Some(guild) = db::guild::get(pool, guild_id).await? {
        if guild.superadmin_user_id == Some(caller_id) {
            return Ok(true);
        }
    }
    if caller_perms
        .map(|p| p.manage_guild() || p.administrator())
        .unwrap_or(false)
    {
        return Ok(true);
    }
    db::permission::check(
        pool,
        guild_id,
        caller_role_ids,
        Action::ManageRuns.as_str(),
        tier_id,
        dungeon_template_id,
    )
    .await
}

/// Convenience wrapper around [`can_organize`] that reads the caller context
/// directly off a `ComponentInteraction`.
pub async fn can_organize_from_interaction(
    pool: &PgPool,
    guild_id: i64,
    mci: &serenity::ComponentInteraction,
    leader_user_id: i64,
    tier_id: Option<i32>,
    dungeon_template_id: Option<i32>,
) -> Result<bool> {
    let caller_id = mci.user.id.get() as i64;
    let (perms, role_ids) = member_meta(mci);
    can_organize(
        pool,
        guild_id,
        caller_id,
        perms,
        &role_ids,
        leader_user_id,
        tier_id,
        dungeon_template_id,
    )
    .await
}

fn member_meta(mci: &serenity::ComponentInteraction) -> (Option<serenity::Permissions>, Vec<i64>) {
    let Some(member) = mci.member.as_ref() else {
        return (None, Vec::new());
    };
    let roles = member.roles.iter().map(|r| r.get() as i64).collect();
    (member.permissions, roles)
}

/// "Trusted operator" check — used by self-organize gates that should
/// bypass anti-troll guardrails (per-user cap, post-cancel cooldown,
/// min-reactors-not-met) for users who have organizer-level trust.
///
/// Returns true when any of these hold:
///   1. caller is the global operator
///   2. caller is the guild's configured superadmin
///   3. caller has Discord "Manage Server" / "Administrator"
///   4. caller has `ManageRuns` granted, scoped to this tier or broader
///
/// **Different from `can_organize`** — does NOT include the
/// "caller is the raid leader" bypass. The leader check makes sense for
/// per-raid lifecycle buttons (Start, Cancel, End) but would defeat the
/// anti-troll cap for self-organize: every caller IS the leader of the
/// raid they're trying to open, so leader-bypass would render the cap
/// useless.
pub async fn is_organizer(
    pool: &PgPool,
    guild_id: i64,
    caller_id: i64,
    caller_perms: Option<serenity::Permissions>,
    caller_role_ids: &[i64],
    tier_id: Option<i32>,
) -> Result<bool> {
    if caller_id == GLOBAL_SUPERADMIN_USER_ID as i64 {
        return Ok(true);
    }
    if let Some(guild) = db::guild::get(pool, guild_id).await? {
        if guild.superadmin_user_id == Some(caller_id) {
            return Ok(true);
        }
    }
    if caller_perms
        .map(|p| p.manage_guild() || p.administrator())
        .unwrap_or(false)
    {
        return Ok(true);
    }
    db::permission::check(
        pool,
        guild_id,
        caller_role_ids,
        Action::ManageRuns.as_str(),
        tier_id,
        None,
    )
    .await
}

/// Convenience wrapper around [`is_organizer`] for `ModalInteraction`
/// callers (modal submissions don't carry a `ComponentInteraction` shape).
pub async fn is_organizer_from_modal(
    pool: &PgPool,
    guild_id: i64,
    modal: &serenity::ModalInteraction,
    tier_id: Option<i32>,
) -> Result<bool> {
    let caller_id = modal.user.id.get() as i64;
    let (perms, roles) = match modal.member.as_ref() {
        Some(m) => (
            m.permissions,
            m.roles.iter().map(|r| r.get() as i64).collect(),
        ),
        None => (None, Vec::new()),
    };
    is_organizer(pool, guild_id, caller_id, perms, &roles, tier_id).await
}
