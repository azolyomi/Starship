use anyhow::{bail, Result};
use crate::{db, BotContext};

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

/// Same as `require` but takes a raw action string (for component handlers
/// where the action is decoded from a custom_id at runtime).
pub async fn require_str(
    ctx: BotContext<'_>,
    action: &str,
    tier_id: Option<i32>,
    dungeon_template_id: Option<i32>,
) -> Result<()> {
    let guild_id = ctx
        .guild_id()
        .ok_or_else(|| anyhow::anyhow!("This command can only be used in a server."))?
        .get() as i64;

    let caller_id = ctx.author().id.get() as i64;

    if let Some(guild) = db::guild::get(&ctx.data().db, guild_id).await? {
        if guild.superadmin_user_id == Some(caller_id) {
            return Ok(());
        }
    }

    let role_ids: Vec<i64> = ctx
        .author_member()
        .await
        .map(|m| m.roles.iter().map(|r| r.get() as i64).collect())
        .unwrap_or_default();

    let allowed = db::permission::check(
        &ctx.data().db,
        guild_id,
        &role_ids,
        action,
        tier_id,
        dungeon_template_id,
    )
    .await?;

    if !allowed {
        bail!("You don't have permission to perform `{action}`.");
    }
    Ok(())
}

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
