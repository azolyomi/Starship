//! `/verify` and `/mv` slash commands.
//!
//! `/verify` mirrors clicking the persistent Verify button: it opens an
//! IGN modal as the slash response. The downstream flow (modal submit →
//! ephemeral with code → check → role+nickname) is handled in
//! `handlers::verify`.
//!
//! `/mv` is the admin-side override. No RealmEye fetch — the admin
//! attests that `target` is `ign`, the bot writes the row, assigns the
//! role, and sets the nickname. Same Discord-side application as a
//! self-verify so /mv survivors get the same nickname-and-role as
//! organic verifiers.

use anyhow::Result;
use poise::serenity_prelude as serenity;
use poise::CreateReply;
use serenity::{CreateInteractionResponse, RoleId, UserId};

use crate::handlers::verify::{build_ign_modal, manual_verify_audit_line, render_manual_outcome};
use crate::services::audit_log;
use crate::services::verification::{
    self, ApplyOutcome, ManualOutcome, NicknameApplyResult, RoleApplyResult,
};
use crate::{db, guild_id_i64, services::permission as perm_svc, BotContext, BotError};

fn ephemeral(msg: impl Into<String>) -> CreateReply {
    CreateReply::default().content(msg).ephemeral(true)
}

// ---------------------------------------------------------------------------
// /verify — self-service entry point
// ---------------------------------------------------------------------------

/// Verify your in-game name with this server.
#[poise::command(slash_command, guild_only)]
pub async fn verify(ctx: BotContext<'_>) -> Result<(), BotError> {
    let guild_id = guild_id_i64(ctx);
    let guild = db::guild::get(&ctx.data().db, guild_id).await?;
    if guild
        .as_ref()
        .map(|g| g.verified_role_id.is_none())
        .unwrap_or(true)
    {
        ctx.send(ephemeral(
            "Verification isn't configured yet — ask an admin to run /setup \
             and pick a Verified role.",
        ))
        .await?;
        return Ok(());
    }

    // Modals can only be sent in response to a non-deferred interaction.
    // Poise's `BotContext` is a wrapper; reach into the application
    // context to get the raw `CommandInteraction` and respond with a
    // modal directly. (Poise has no helper for "respond with modal".)
    let app_ctx = match ctx {
        poise::Context::Application(app) => app,
        poise::Context::Prefix(_) => {
            // /verify is declared `slash_command` only, so this branch
            // is unreachable in practice.
            ctx.send(ephemeral("/verify can only be used as a slash command."))
                .await?;
            return Ok(());
        }
    };

    app_ctx
        .interaction
        .create_response(
            ctx.http(),
            CreateInteractionResponse::Modal(build_ign_modal()),
        )
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// /mv — admin manual verify
// ---------------------------------------------------------------------------

/// Manually verify a user (admin override). Skips the RealmEye check.
#[poise::command(
    slash_command,
    guild_only,
    rename = "mv",
    default_member_permissions = "MANAGE_GUILD"
)]
pub async fn manual_verify(
    ctx: BotContext<'_>,
    #[description = "Discord user to verify"] user: serenity::User,
    #[description = "Their in-game name"] ign: String,
) -> Result<(), BotError> {
    perm_svc::require(ctx, perm_svc::Action::ConfigureGuild, None, None).await?;

    let guild_id_i = guild_id_i64(ctx);
    let pool = &ctx.data().db;

    let Some(guild) = db::guild::get(pool, guild_id_i).await? else {
        ctx.send(ephemeral(
            "This server hasn't been set up yet. Run /setup first.",
        ))
        .await?;
        return Ok(());
    };
    let Some(role_raw) = guild.verified_role_id else {
        ctx.send(ephemeral(
            "Verification isn't configured yet — finish /setup's verification \
             section first.",
        ))
        .await?;
        return Ok(());
    };

    let trimmed = ign.trim();
    if trimmed.is_empty() {
        ctx.send(ephemeral("Please provide an in-game name."))
            .await?;
        return Ok(());
    }
    if !trimmed.chars().all(|c| c.is_ascii_alphabetic()) || trimmed.len() > 20 {
        ctx.send(ephemeral(
            "That doesn't look like a RotMG IGN — should be 1-12 letters, \
             alphabetic only.",
        ))
        .await?;
        return Ok(());
    }
    if user.bot {
        ctx.send(ephemeral("Bots can't be verified.")).await?;
        return Ok(());
    }

    // /mv runs full DB transactions + role assignment, both of which can
    // exceed the 3s interaction window in worst case (rate-limited add).
    ctx.defer_ephemeral().await?;

    let admin_id = ctx.author().id.get() as i64;
    let outcome =
        verification::manual_verify(pool, guild_id_i, user.id.get() as i64, trimmed, admin_id)
            .await?;

    let summary = match &outcome {
        ManualOutcome::IgnTaken { .. } => {
            render_manual_outcome(
                // ApplyOutcome unused on the IgnTaken branch; pass a placeholder.
                &ApplyOutcome {
                    role: RoleApplyResult::Ok,
                    nickname: NicknameApplyResult::Ok,
                },
                trimmed,
                &outcome,
            )
            .await
        }
        ManualOutcome::Verified { .. } => {
            let guild_id = serenity::GuildId::new(guild_id_i as u64);
            let apply = verification::apply_verified_state(
                ctx.http(),
                guild_id,
                UserId::new(user.id.get()),
                trimmed,
                RoleId::new(role_raw as u64),
            )
            .await;
            render_manual_outcome(&apply, trimmed, &outcome).await
        }
    };

    if let Some(line) = manual_verify_audit_line(admin_id as u64, user.id.get(), trimmed, &outcome)
    {
        audit_log::post(ctx.http(), pool, guild_id_i, line).await;
    }

    ctx.send(ephemeral(format!("**<@{}>**\n{summary}", user.id)))
        .await?;
    Ok(())
}
