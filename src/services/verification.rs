//! High-level verification orchestration.
//!
//! Three top-level operations:
//!   * [`issue_code`] — generate a 6-digit code and persist a pending row.
//!     Replaces any prior pending attempt for the same user.
//!   * [`complete`] — fetch the user's RealmEye page, look for the
//!     pending code, atomically swap pending → verified.
//!   * [`manual_verify`] — admin override (`/mv`); skips the RealmEye
//!     fetch entirely.
//!
//! Plus [`apply_verified_state`], the Discord-side application of a
//! successful verification: assigns the verified role and sets the
//! user's server nickname to their IGN. This is a separate function so
//! the DB write stays unconditional — if the role/nickname call fails
//! (e.g. bot role below the verified role), the user can click "I added
//! it" again and the idempotent UPSERT short-circuits without re-hitting
//! RealmEye.

use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use poise::serenity_prelude as serenity;
use rand::Rng as _;
use serenity::{EditMember, GuildId, Http, RoleId, UserId};
use sqlx::PgPool;
use tracing::warn;

use crate::db;
use crate::services::realmeye::{self, LookupResult, RealmEyeClient};

/// How long a pending verification stays valid. Discord ephemerals expire
/// at 15 minutes (interaction-token TTL); we give the pending row a
/// longer life so a user who closed the ephemeral can rerun /verify with
/// the same IGN and pick up where they left off without surprises.
pub const CODE_TTL: chrono::Duration = chrono::Duration::minutes(30);

/// Length of the user-visible code. RealmEye descriptions are short, six
/// digits is enough to avoid accidental collisions with natural text,
/// and they fit on one mobile keyboard tap each.
const CODE_DIGITS: u32 = 6;

/// Number of role-assignment retries before giving up. Mirrors the
/// pattern in `commands/pingroles.rs::mutate_role_with_retry`.
const ROLE_RETRY_ATTEMPTS: usize = 3;

// ---------------------------------------------------------------------------
// issue_code
// ---------------------------------------------------------------------------

/// Generate a fresh code for `(guild, user, ign)`, persist it, and return
/// it for rendering. Overwrites any prior pending row for the same user.
pub async fn issue_code(
    pool: &PgPool,
    guild_id: i64,
    discord_user_id: i64,
    claimed_ign: &str,
) -> Result<String> {
    let code = generate_code();
    let expires_at = Utc::now() + CODE_TTL;
    db::verification::upsert_pending(
        pool,
        guild_id,
        discord_user_id,
        claimed_ign,
        &code,
        expires_at,
    )
    .await?;
    Ok(code)
}

/// 6-digit zero-padded random string. `format!("{:06}", n % 1_000_000)`
/// preserves leading zeros (the row stores TEXT, not INT).
fn generate_code() -> String {
    let n: u32 = rand::thread_rng().gen_range(0..1_000_000);
    format!("{n:0width$}", width = CODE_DIGITS as usize)
}

// ---------------------------------------------------------------------------
// complete
// ---------------------------------------------------------------------------

/// Verification check outcome — what the handler renders to the user.
///
/// Distinct cases let the handler write precise messages without parsing
/// the result. The boundary between "the user is now verified" and "the
/// user is not" is exactly the [`Verified`] variant; everything else
/// leaves the pending row in place so the user can click *I added it*
/// again or *New code* to retry.
///
/// [`Verified`]: Outcome::Verified
#[derive(Debug)]
pub enum Outcome {
    /// Code matched. Pending row deleted, `verified_users` UPSERTed.
    /// `canonical_ign` is the case-correct IGN from RealmEye's `<h1>`.
    Verified {
        canonical_ign: String,
        /// Information for the success message — first-time, refresh, or
        /// rebind from a prior IGN.
        kind: VerifiedKind,
    },
    /// No pending row for this user — they need to start over.
    NoPending,
    /// Pending row's `expires_at` has passed. Deleted; user must click
    /// *New code*.
    Expired,
    /// RealmEye rendered the page but the code wasn't in the
    /// description. User probably forgot to save the description.
    CodeMissing { canonical_ign: String },
    /// RealmEye rendered the page but the description block is hidden /
    /// empty (private profile).
    Private { canonical_ign: String },
    /// RealmEye returned 404 for the IGN.
    NotFound,
    /// RealmEye is throttling us or temporarily down.
    Throttled,
    /// Network failure or unexpected RealmEye state. Log + tell the user
    /// to retry; admin can `/mv` if it persists.
    RealmEyeUnavailable,
    /// Another Discord user in this guild already holds this IGN.
    /// `holder` is their Discord user ID for the error message.
    IgnTaken { holder: i64 },
}

/// Refinement of [`Outcome::Verified`]. Lets the success embed use the
/// right verb ("Verified" vs "Re-verified" vs "Rebound from X to Y").
#[derive(Debug)]
pub enum VerifiedKind {
    Created,
    Refreshed,
    Rebound { from: String },
}

/// User clicked *I added it*. Read the pending row, fetch RealmEye, and
/// — if the code is in the description — atomically commit the
/// verification.
pub async fn complete(
    pool: &PgPool,
    realmeye: &RealmEyeClient,
    guild_id: i64,
    discord_user_id: i64,
) -> Result<Outcome> {
    let Some(pending) = db::verification::get_pending(pool, guild_id, discord_user_id).await?
    else {
        return Ok(Outcome::NoPending);
    };

    if pending.expires_at <= Utc::now() {
        // Drop the stale row so the next /verify can repopulate cleanly.
        db::verification::delete_pending(pool, guild_id, discord_user_id).await?;
        return Ok(Outcome::Expired);
    }

    match realmeye.lookup_player(&pending.claimed_ign).await {
        LookupResult::Found {
            description,
            canonical_ign,
        } => {
            if !realmeye::description_contains_code(&description, &pending.code) {
                return Ok(Outcome::CodeMissing { canonical_ign });
            }
            commit_verified(pool, guild_id, discord_user_id, &canonical_ign, None).await
        }
        LookupResult::Private { canonical_ign } => Ok(Outcome::Private { canonical_ign }),
        LookupResult::NotFound => Ok(Outcome::NotFound),
        LookupResult::Throttled => Ok(Outcome::Throttled),
        LookupResult::TransportError(e) => {
            warn!(
                error = ?e,
                ign = %pending.claimed_ign,
                "RealmEye lookup failed during verification check",
            );
            Ok(Outcome::RealmEyeUnavailable)
        }
    }
}

// ---------------------------------------------------------------------------
// manual_verify
// ---------------------------------------------------------------------------

/// Outcome of [`manual_verify`]. Narrower than [`Outcome`] because there's
/// no RealmEye fetch — only the DB write can fail meaningfully.
#[derive(Debug)]
pub enum ManualOutcome {
    Verified {
        kind: VerifiedKind,
    },
    /// Same as [`Outcome::IgnTaken`] but reachable via `/mv`.
    IgnTaken {
        holder: i64,
    },
}

/// Admin-side `/mv @user <ign>` — verify without checking RealmEye.
/// Records `verified_by = Some(admin_user_id)` for the audit trail.
pub async fn manual_verify(
    pool: &PgPool,
    guild_id: i64,
    target_user_id: i64,
    ign: &str,
    admin_user_id: i64,
) -> Result<ManualOutcome> {
    let result =
        db::verification::complete(pool, guild_id, target_user_id, ign, Some(admin_user_id))
            .await?;
    Ok(match result {
        db::verification::UpsertResult::Created => ManualOutcome::Verified {
            kind: VerifiedKind::Created,
        },
        db::verification::UpsertResult::Refreshed => ManualOutcome::Verified {
            kind: VerifiedKind::Refreshed,
        },
        db::verification::UpsertResult::Rebound { from } => ManualOutcome::Verified {
            kind: VerifiedKind::Rebound { from },
        },
        db::verification::UpsertResult::IgnTaken { holder } => ManualOutcome::IgnTaken { holder },
    })
}

/// Shared commit path between [`complete`] (self-verify) and a future
/// `/whois`-style admin tool. `verified_by` is `None` for self-verifies,
/// `Some(admin_user_id)` for `/mv`. Returns the same [`Outcome`] variants
/// the caller already speaks.
async fn commit_verified(
    pool: &PgPool,
    guild_id: i64,
    discord_user_id: i64,
    canonical_ign: &str,
    verified_by: Option<i64>,
) -> Result<Outcome> {
    let result =
        db::verification::complete(pool, guild_id, discord_user_id, canonical_ign, verified_by)
            .await?;
    Ok(match result {
        db::verification::UpsertResult::Created => Outcome::Verified {
            canonical_ign: canonical_ign.to_string(),
            kind: VerifiedKind::Created,
        },
        db::verification::UpsertResult::Refreshed => Outcome::Verified {
            canonical_ign: canonical_ign.to_string(),
            kind: VerifiedKind::Refreshed,
        },
        db::verification::UpsertResult::Rebound { from } => Outcome::Verified {
            canonical_ign: canonical_ign.to_string(),
            kind: VerifiedKind::Rebound { from },
        },
        db::verification::UpsertResult::IgnTaken { holder } => Outcome::IgnTaken { holder },
    })
}

// ---------------------------------------------------------------------------
// apply_verified_state — Discord-side: role + nickname
// ---------------------------------------------------------------------------

/// Result of applying the post-verification Discord state. Both fields are
/// independent: a role-assign failure does NOT prevent the nickname-set
/// attempt, and vice versa. The caller surfaces both in the success
/// message.
#[derive(Debug)]
pub struct ApplyOutcome {
    pub role: RoleApplyResult,
    pub nickname: NicknameApplyResult,
}

#[derive(Debug)]
pub enum RoleApplyResult {
    /// Role added (or already held — Discord treats add of an existing
    /// role as a no-op 204).
    Ok,
    /// All retries failed. Almost always means the bot's highest role is
    /// below the verified role in the guild's role hierarchy.
    Failed { reason: String },
}

#[derive(Debug)]
pub enum NicknameApplyResult {
    /// Nickname set (or already correct).
    Ok,
    /// Bot lacks permission (role hierarchy / Manage Nicknames /
    /// targeting the server owner). Non-fatal.
    Skipped { reason: String },
}

/// Assign the verified role and set the user's nickname to their IGN.
/// Best-effort: a failure on either branch is logged + reported but does
/// not roll back the DB write. Idempotent — a re-run of /verify safely
/// re-applies both.
pub async fn apply_verified_state(
    http: &Http,
    guild_id: GuildId,
    user_id: UserId,
    ign: &str,
    verified_role_id: RoleId,
) -> ApplyOutcome {
    let role = assign_role(http, guild_id, user_id, verified_role_id).await;
    let nickname = set_nickname(http, guild_id, user_id, ign).await;
    ApplyOutcome { role, nickname }
}

async fn assign_role(
    http: &Http,
    guild_id: GuildId,
    user_id: UserId,
    role_id: RoleId,
) -> RoleApplyResult {
    let mut last_err: Option<String> = None;
    for attempt in 0..ROLE_RETRY_ATTEMPTS {
        match http
            .add_member_role(guild_id, user_id, role_id, Some("Starship verification"))
            .await
        {
            Ok(()) => return RoleApplyResult::Ok,
            Err(e) => {
                last_err = Some(format!("{e:#}"));
                // 200 → 400 → 800 ms backoff. Mirrors
                // `commands/pingroles.rs::mutate_role_with_retry`.
                if attempt + 1 < ROLE_RETRY_ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(200u64 << attempt)).await;
                }
            }
        }
    }
    RoleApplyResult::Failed {
        reason: last_err.unwrap_or_else(|| "unknown error".to_string()),
    }
}

async fn set_nickname(
    http: &Http,
    guild_id: GuildId,
    user_id: UserId,
    nickname: &str,
) -> NicknameApplyResult {
    // Discord nicknames cap at 32 chars. RealmEye IGNs cap at 12, so this
    // is a defensive truncate against a future schema change rather than
    // an expected branch.
    let n: String = nickname.chars().take(32).collect();
    match guild_id
        .edit_member(http, user_id, EditMember::new().nickname(&n))
        .await
    {
        Ok(_) => NicknameApplyResult::Ok,
        Err(e) => {
            // Don't bubble — a missing nickname is a far smaller harm
            // than a failed verification. The user-facing message will
            // say "you're verified, but I couldn't set your nickname —
            // ask a mod" so it's still actionable.
            warn!(
                error = ?e,
                guild_id = guild_id.get(),
                user_id = user_id.get(),
                "could not set nickname on verification",
            );
            NicknameApplyResult::Skipped {
                reason: format!("{e:#}"),
            }
        }
    }
}
