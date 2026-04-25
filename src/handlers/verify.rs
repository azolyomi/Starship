//! Component + modal routing for `verify:*` custom_ids.
//!
//! Stateless protocol — every handler reads its state from the DB so the
//! flow survives restarts and the persistent Verify button keeps working
//! across deploys.
//!
//! Custom-id grammar:
//!   verify:start          — button on the persistent message (channel)
//!                           or /verify slash command. Opens IGN modal.
//!   verify:submit_ign     — modal submission. Issues a 6-digit code,
//!                           writes the pending row, replies with an
//!                           ephemeral that carries the code and two
//!                           buttons.
//!   verify:check          — "I added it" button on the ephemeral.
//!                           Fetches the user's RealmEye page, looks for
//!                           the code, atomically commits on match.
//!   verify:resend         — "New code" button on the ephemeral. Issues
//!                           a fresh code for the same IGN.
//!
//! All handler responses are ephemeral so the IGN + code stay private.
//! Discord guarantees that ephemeral message buttons can only be clicked
//! by the user the original interaction belonged to, so no caller-id
//! check is needed on the per-attempt buttons.

use poise::serenity_prelude as serenity;
use serenity::{
    ActionRowComponent, ButtonStyle, CreateActionRow, CreateButton, CreateEmbed, CreateInputText,
    CreateInteractionResponse, CreateInteractionResponseMessage, CreateModal, GuildId,
    InputTextStyle, RoleId,
};

use crate::services::audit_log;
use crate::services::verification::{
    self, ApplyOutcome, ManualOutcome, NicknameApplyResult, Outcome, RoleApplyResult, VerifiedKind,
};
use crate::{db, BotData, BotError};

const MODAL_CUSTOM_ID: &str = "verify:submit_ign";
const IGN_INPUT_ID: &str = "ign";

// ---------------------------------------------------------------------------
// Top-level dispatchers
// ---------------------------------------------------------------------------

pub async fn handle_component(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
) -> Result<(), BotError> {
    match mci.data.custom_id.as_str() {
        "verify:start" => handle_start(ctx, mci, data).await,
        "verify:check" => handle_check(ctx, mci, data).await,
        "verify:resend" => handle_resend(ctx, mci, data).await,
        _ => Ok(()),
    }
}

pub async fn handle_modal(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    data: &BotData,
) -> Result<(), BotError> {
    if modal.data.custom_id == MODAL_CUSTOM_ID {
        handle_submit_ign(ctx, modal, data).await
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// verify:start — open the IGN modal
// ---------------------------------------------------------------------------

async fn handle_start(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
) -> Result<(), BotError> {
    let Some(guild_id) = mci.guild_id else {
        return respond_ephemeral_text(ctx, mci, "Verification only works in a server.").await;
    };
    if !ensure_verification_configured(ctx, mci, data, guild_id).await? {
        return Ok(());
    }
    open_ign_modal(ctx, mci).await
}

/// Build the IGN-input modal. Used both by the persistent button click
/// path (here) and by the `/verify` slash command.
pub fn build_ign_modal() -> CreateModal {
    CreateModal::new(MODAL_CUSTOM_ID, "Verify with RealmEye").components(vec![
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Short, "In-game name (IGN)", IGN_INPUT_ID)
                .placeholder("Your RotMG character name as it appears on RealmEye")
                .required(true)
                .min_length(1)
                .max_length(20),
        ),
    ])
}

async fn open_ign_modal(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
) -> Result<(), BotError> {
    mci.create_response(ctx, CreateInteractionResponse::Modal(build_ign_modal()))
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// verify:submit_ign — modal submission
// ---------------------------------------------------------------------------

async fn handle_submit_ign(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    data: &BotData,
) -> Result<(), BotError> {
    let Some(guild_id) = modal.guild_id else {
        return respond_modal_text(ctx, modal, "Verification only works in a server.").await;
    };
    if !modal_ensure_verification_configured(ctx, modal, data, guild_id).await? {
        return Ok(());
    }

    let raw_ign = match extract_input(modal, IGN_INPUT_ID) {
        Some(s) => s.trim().to_string(),
        None => {
            return respond_modal_text(ctx, modal, "No IGN provided. Try again.").await;
        }
    };
    if raw_ign.is_empty() {
        return respond_modal_text(ctx, modal, "Please enter your in-game name.").await;
    }
    if !is_plausible_ign(&raw_ign) {
        return respond_modal_text(
            ctx,
            modal,
            "That doesn't look like a RotMG IGN — it should be 1-12 letters.",
        )
        .await;
    }

    let code = verification::issue_code(
        &data.db,
        guild_id.get() as i64,
        modal.user.id.get() as i64,
        &raw_ign,
    )
    .await?;

    modal
        .create_response(
            ctx,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .embed(code_embed(&raw_ign, &code))
                    .components(check_buttons())
                    .ephemeral(true),
            ),
        )
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// verify:check — fetch RealmEye and finalise
// ---------------------------------------------------------------------------

async fn handle_check(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
) -> Result<(), BotError> {
    let Some(guild_id) = mci.guild_id else {
        return respond_ephemeral_text(ctx, mci, "Verification only works in a server.").await;
    };

    // RealmEye latency can spike past Discord's 3s interaction window.
    // Defer the response so the button stops showing "thinking" promptly
    // and we can edit later.
    mci.defer_ephemeral(ctx).await?;

    let user_id = mci.user.id.get() as i64;
    let outcome =
        verification::complete(&data.db, &data.realmeye, guild_id.get() as i64, user_id).await?;

    match outcome {
        Outcome::Verified {
            canonical_ign,
            kind,
        } => {
            let role = read_verified_role(&data.db, guild_id).await?;
            let apply = match role {
                Some(role_id) => {
                    verification::apply_verified_state(
                        &ctx.http,
                        guild_id,
                        mci.user.id,
                        &canonical_ign,
                        role_id,
                    )
                    .await
                }
                None => ApplyOutcome {
                    role: RoleApplyResult::Failed {
                        reason: "no Verified role configured".to_string(),
                    },
                    nickname: NicknameApplyResult::Skipped {
                        reason: "no Verified role configured".to_string(),
                    },
                },
            };
            audit_log::post(
                &ctx.http,
                &data.db,
                guild_id.get() as i64,
                self_verify_audit_line(mci.user.id.get(), &canonical_ign, &kind),
            )
            .await;
            edit_to_success(ctx, mci, &canonical_ign, &kind, &apply).await
        }
        Outcome::NoPending => {
            edit_to_text(
                ctx,
                mci,
                "No pending verification — start over with /verify or the Verify button.",
                false,
            )
            .await
        }
        Outcome::Expired => {
            edit_to_text(
                ctx,
                mci,
                "Your code expired. Run /verify again to get a fresh one.",
                false,
            )
            .await
        }
        Outcome::CodeMissing { canonical_ign } => {
            edit_to_text(
                ctx,
                mci,
                &format!(
                "Couldn't find your code in <https://www.realmeye.com/player/{canonical_ign}>'s \
                     description. Make sure you saved the description and that your **profile \
                     is set to public** (RealmEye → Settings → Privacy), then click \
                     **I added it** again."
            ),
                true,
            )
            .await
        }
        Outcome::Private { canonical_ign } => {
            edit_to_text(
                ctx,
                mci,
                &format!(
                    "Your RealmEye profile for **{canonical_ign}** is private (or your \
                     description is empty). Set your **profile to public** (RealmEye → \
                     Settings → Privacy), save your description, and click **I added it** again."
                ),
                true,
            )
            .await
        }
        Outcome::NotFound => {
            edit_to_text(
                ctx,
                mci,
                "RealmEye doesn't have a player by that name. Make sure your in-game name is \
                 spelled exactly right and that you've logged into the game on this account at \
                 least once recently — RealmEye doesn't show characters that have never been \
                 seen.",
                false,
            )
            .await
        }
        Outcome::Throttled | Outcome::RealmEyeUnavailable => {
            edit_to_text(
                ctx,
                mci,
                "Couldn't reach RealmEye right now. Try **I added it** again in a minute. \
                 If this keeps failing, ask a moderator to verify you manually.",
                true,
            )
            .await
        }
        Outcome::IgnTaken { holder } => {
            edit_to_text(
                ctx,
                mci,
                &format!(
                    "**<@{holder}>** is already verified with that in-game name in this server. \
                     If that's a mistake, ping a moderator."
                ),
                false,
            )
            .await
        }
    }
}

// ---------------------------------------------------------------------------
// verify:resend — re-issue a code for the same IGN
// ---------------------------------------------------------------------------

async fn handle_resend(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
) -> Result<(), BotError> {
    let Some(guild_id) = mci.guild_id else {
        return respond_ephemeral_text(ctx, mci, "Verification only works in a server.").await;
    };

    let user_id = mci.user.id.get() as i64;
    let pending = db::verification::get_pending(&data.db, guild_id.get() as i64, user_id).await?;
    let Some(pending) = pending else {
        return edit_to_text(
            ctx,
            mci,
            "No pending verification — start over with /verify or the Verify button.",
            false,
        )
        .await;
    };

    let code = verification::issue_code(
        &data.db,
        guild_id.get() as i64,
        user_id,
        &pending.claimed_ign,
    )
    .await?;

    mci.create_response(
        ctx,
        CreateInteractionResponse::UpdateMessage(
            CreateInteractionResponseMessage::new()
                .embed(code_embed(&pending.claimed_ign, &code))
                .components(check_buttons()),
        ),
    )
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared rendering
// ---------------------------------------------------------------------------

fn code_embed(claimed_ign: &str, code: &str) -> CreateEmbed {
    CreateEmbed::new()
        .title("🔐 Verification code")
        .description(format!(
            "Almost there. To prove **{claimed_ign}** is your in-game name:\n\
             \n\
             1. Open <https://www.realmeye.com/player/{claimed_ign}> and log in.\n\
             2. Edit your description and paste this code:\n\
             ```\n{code}\n```\n\
             3. Save the description. Make sure your **profile is set to public** \
                (RealmEye → Settings → Privacy) — that controls description visibility too.\n\
             4. Click **I added it** below.\n\
             \n\
             You can remove the code from your description as soon as \
             verification succeeds. The code expires in 30 minutes — click \
             **New code** if it does."
        ))
        .color(0x5865F2)
}

fn check_buttons() -> Vec<CreateActionRow> {
    vec![CreateActionRow::Buttons(vec![
        CreateButton::new("verify:check")
            .label("I added it")
            .style(ButtonStyle::Success),
        CreateButton::new("verify:resend")
            .label("New code")
            .style(ButtonStyle::Secondary),
    ])]
}

async fn edit_to_success(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    canonical_ign: &str,
    kind: &VerifiedKind,
    apply: &ApplyOutcome,
) -> Result<(), BotError> {
    let mut lines = vec![match kind {
        VerifiedKind::Created => format!("✅ Verified as **{canonical_ign}**."),
        VerifiedKind::Refreshed => format!("✅ Re-verified as **{canonical_ign}**."),
        VerifiedKind::Rebound { from } => {
            format!("✅ Verified as **{canonical_ign}** (replaces your previous IGN, _{from}_).")
        }
    }];

    match &apply.role {
        RoleApplyResult::Ok => lines.push("• Verified role: granted.".to_string()),
        RoleApplyResult::Failed { reason } => lines.push(format!(
            "• Verified role: **could not assign** — {reason}. \
             Ask a moderator to grant it manually."
        )),
    }
    match &apply.nickname {
        NicknameApplyResult::Ok => lines.push(format!("• Nickname: set to **{canonical_ign}**.")),
        NicknameApplyResult::Skipped { reason } => lines.push(format!(
            "• Nickname: not changed — {reason}. \
             You can set it yourself in **Edit Server Profile**."
        )),
    }
    lines.push("\nThanks for verifying! You can dismiss this message.".to_string());

    let embed = CreateEmbed::new()
        .title("🔐 Verification complete")
        .description(lines.join("\n"))
        .color(0x57F287);

    mci.edit_response(
        ctx,
        serenity::EditInteractionResponse::new()
            .embed(embed)
            .components(vec![]),
    )
    .await?;
    Ok(())
}

/// Edit the ephemeral to a flat message. `keep_buttons = true` for cases
/// the user can recover from in place ("you forgot to save the
/// description, click again"); `false` for terminal states.
async fn edit_to_text(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    text: &str,
    keep_buttons: bool,
) -> Result<(), BotError> {
    let mut edit = serenity::EditInteractionResponse::new()
        .embeds(vec![])
        .content(text);
    edit = if keep_buttons {
        edit.components(check_buttons())
    } else {
        edit.components(vec![])
    };
    mci.edit_response(ctx, edit).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// True if the verification config is in place. Otherwise replies with a
/// friendly ephemeral and returns false (caller should bail).
async fn ensure_verification_configured(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    data: &BotData,
    guild_id: GuildId,
) -> Result<bool, BotError> {
    let guild = match db::guild::get(&data.db, guild_id.get() as i64).await? {
        Some(g) => g,
        None => {
            respond_ephemeral_text(
                ctx,
                mci,
                "This server hasn't been set up yet. Ask an admin to run /setup.",
            )
            .await?;
            return Ok(false);
        }
    };
    if guild.verified_role_id.is_none() {
        respond_ephemeral_text(
            ctx,
            mci,
            "Verification isn't configured yet — ask an admin to run /setup and \
             pick a Verified role.",
        )
        .await?;
        return Ok(false);
    }
    Ok(true)
}

async fn modal_ensure_verification_configured(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    data: &BotData,
    guild_id: GuildId,
) -> Result<bool, BotError> {
    let guild = match db::guild::get(&data.db, guild_id.get() as i64).await? {
        Some(g) => g,
        None => {
            respond_modal_text(
                ctx,
                modal,
                "This server hasn't been set up yet. Ask an admin to run /setup.",
            )
            .await?;
            return Ok(false);
        }
    };
    if guild.verified_role_id.is_none() {
        respond_modal_text(
            ctx,
            modal,
            "Verification isn't configured — ask an admin to run /setup.",
        )
        .await?;
        return Ok(false);
    }
    Ok(true)
}

async fn read_verified_role(
    pool: &sqlx::PgPool,
    guild_id: GuildId,
) -> Result<Option<RoleId>, BotError> {
    let guild = db::guild::get(pool, guild_id.get() as i64).await?;
    Ok(guild
        .and_then(|g| g.verified_role_id)
        .map(|r| RoleId::new(r as u64)))
}

async fn respond_ephemeral_text(
    ctx: &serenity::Context,
    mci: &serenity::ComponentInteraction,
    text: &str,
) -> Result<(), BotError> {
    mci.create_response(
        ctx,
        CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content(text)
                .ephemeral(true),
        ),
    )
    .await?;
    Ok(())
}

async fn respond_modal_text(
    ctx: &serenity::Context,
    modal: &serenity::ModalInteraction,
    text: &str,
) -> Result<(), BotError> {
    modal
        .create_response(
            ctx,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(text)
                    .ephemeral(true),
            ),
        )
        .await?;
    Ok(())
}

fn extract_input(modal: &serenity::ModalInteraction, custom_id: &str) -> Option<String> {
    for row in &modal.data.components {
        for comp in &row.components {
            if let ActionRowComponent::InputText(it) = comp {
                if it.custom_id == custom_id {
                    return it.value.clone();
                }
            }
        }
    }
    None
}

/// Cheap structural check on user-typed IGN. RealmEye allows `[A-Za-z]`
/// up to 10-12 chars in practice. We accept slightly looser (1-20) so
/// that names with edge-case casing don't bounce, but reject strings
/// that obviously can't be a RotMG IGN.
fn is_plausible_ign(s: &str) -> bool {
    s.len() <= 20 && s.chars().all(|c| c.is_ascii_alphabetic())
}

// ---------------------------------------------------------------------------
// Public re-export for /mv (admin) and /verify (slash command).
// ---------------------------------------------------------------------------

/// Apply a successful manual_verify outcome from `/mv`. Centralised here
/// so the manual + self-verify success paths render the role/nickname
/// summary identically.
pub async fn render_manual_outcome(
    apply: &ApplyOutcome,
    canonical_ign: &str,
    kind: &ManualOutcome,
) -> String {
    let mut lines = vec![match kind {
        ManualOutcome::Verified {
            kind: VerifiedKind::Created,
        } => format!("✅ Manually verified as **{canonical_ign}**."),
        ManualOutcome::Verified {
            kind: VerifiedKind::Refreshed,
        } => format!("✅ Re-verified as **{canonical_ign}**."),
        ManualOutcome::Verified {
            kind: VerifiedKind::Rebound { from },
        } => format!(
            "✅ Manually verified as **{canonical_ign}** (replaces previous IGN, _{from}_)."
        ),
        ManualOutcome::IgnTaken { holder } => {
            return format!("**<@{holder}>** is already verified with that in-game name.");
        }
    }];

    match &apply.role {
        RoleApplyResult::Ok => lines.push("• Verified role: granted.".to_string()),
        RoleApplyResult::Failed { reason } => {
            lines.push(format!("• Verified role: **could not assign** — {reason}."))
        }
    }
    match &apply.nickname {
        NicknameApplyResult::Ok => lines.push(format!("• Nickname: set to **{canonical_ign}**.")),
        NicknameApplyResult::Skipped { reason } => {
            lines.push(format!("• Nickname: not changed — {reason}."))
        }
    }

    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Audit-log line formatters
// ---------------------------------------------------------------------------

/// Plain-text summary of a self-verify success, posted to the guild's
/// configured log channel when present. Kept terse so a long log stays
/// scannable; <@mention> + backtick'd IGN render cleanly inline.
pub fn self_verify_audit_line(user_id: u64, canonical_ign: &str, kind: &VerifiedKind) -> String {
    match kind {
        VerifiedKind::Created => {
            format!("🔐 Verified: <@{user_id}> as `{canonical_ign}`")
        }
        VerifiedKind::Refreshed => {
            format!("🔐 Re-verified: <@{user_id}> refreshed as `{canonical_ign}`")
        }
        VerifiedKind::Rebound { from } => {
            format!("🔐 Rebind: <@{user_id}> changed IGN from `{from}` to `{canonical_ign}`")
        }
    }
}

/// Plain-text summary of a manual `/mv` success. Same shape as
/// [`self_verify_audit_line`] but names the admin who attested.
pub fn manual_verify_audit_line(
    admin_id: u64,
    target_id: u64,
    canonical_ign: &str,
    kind: &ManualOutcome,
) -> Option<String> {
    let inner_kind = match kind {
        ManualOutcome::Verified { kind } => kind,
        ManualOutcome::IgnTaken { .. } => return None,
    };
    let body = match inner_kind {
        VerifiedKind::Created => {
            format!("<@{target_id}> as `{canonical_ign}`")
        }
        VerifiedKind::Refreshed => {
            format!("<@{target_id}> refreshed as `{canonical_ign}`")
        }
        VerifiedKind::Rebound { from } => {
            format!("<@{target_id}> rebound from `{from}` to `{canonical_ign}`")
        }
    };
    Some(format!("🔐 Manual-verified by <@{admin_id}>: {body}"))
}
