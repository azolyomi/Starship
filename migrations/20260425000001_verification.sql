-- Verification: linking a Discord user to their RealmEye in-game name (IGN).
--
-- The flow:
--   1. User clicks Verify (or runs /verify), enters their IGN.
--   2. Bot issues a 6-digit code, written to `verifications` (pending).
--   3. User pastes the code into their RealmEye description.
--   4. User clicks "I added it"; bot scrapes realmeye.com/player/<ign>,
--      finds the code, atomically deletes the pending row and UPSERTs
--      `verified_users`. Then assigns a "Verified" role and sets the
--      Discord nickname to the IGN.
--
-- Verification is per-server: a row in `verified_users` binds a Discord
-- user to an IGN within one guild only. PK is (guild_id, discord_user_id),
-- so re-running /verify silently rebinds. UNIQUE (guild_id, realmeye_ign)
-- enforces "one IGN per server" — catches alts at the schema level.
--
-- `/setup` configures three new fields on guilds: which role marks a
-- user as verified, which channel the persistent Verify button lives in,
-- and the message ID of that button (so the startup sweep can detect
-- when the message was deleted out from under us).

-- ---------------------------------------------------------------------------
-- guilds: per-guild verification config.
-- All three nullable. A guild is "verification-ready" iff
-- verified_role_id IS NOT NULL.
-- ---------------------------------------------------------------------------
ALTER TABLE guilds
    ADD COLUMN verified_role_id   BIGINT,
    ADD COLUMN verify_channel_id  BIGINT,
    ADD COLUMN verify_message_id  BIGINT;

-- ---------------------------------------------------------------------------
-- verifications: pending flow rows.
-- Auto-cleaned on success (delete) or expiry (startup sweep + replace on
-- next /verify). PK on (guild, user) means a fresh /verify silently
-- overwrites any prior pending attempt for that user.
-- ---------------------------------------------------------------------------
CREATE TABLE verifications (
    guild_id          BIGINT       NOT NULL REFERENCES guilds(guild_id) ON DELETE CASCADE,
    discord_user_id   BIGINT       NOT NULL,
    claimed_ign       TEXT         NOT NULL,
    -- 6-digit string, leading zeros preserved (TEXT not INT).
    code              TEXT         NOT NULL,
    expires_at        TIMESTAMPTZ  NOT NULL,
    created_at        TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    PRIMARY KEY (guild_id, discord_user_id)
);

CREATE INDEX idx_verifications_expires ON verifications(expires_at);

-- ---------------------------------------------------------------------------
-- verified_users: completed verifications.
-- UNIQUE (guild_id, realmeye_ign) is the schema-level alt check — at most
-- one Discord user holds a given IGN in any one server. `verified_by` is
-- NULL for self-verifies, set to the admin's user_id for /mv (manual).
-- ---------------------------------------------------------------------------
CREATE TABLE verified_users (
    guild_id          BIGINT       NOT NULL REFERENCES guilds(guild_id) ON DELETE CASCADE,
    discord_user_id   BIGINT       NOT NULL,
    realmeye_ign      TEXT         NOT NULL,
    verified_at       TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    verified_by       BIGINT,
    PRIMARY KEY (guild_id, discord_user_id),
    UNIQUE (guild_id, realmeye_ign)
);

CREATE INDEX idx_verified_users_ign ON verified_users(guild_id, realmeye_ign);
