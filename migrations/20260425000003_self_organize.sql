-- ---------------------------------------------------------------------------
-- Self-organized raids: per-tier opt-in mode where any user can start a
-- headcount via a sticky button (no leader role required), with anti-troll
-- guardrails: per-(tier, dungeon) slot uniqueness, idle auto-cancel,
-- guild-wide one-active-raid-per-user cap, post-cancel cooldown, and a
-- minimum-reactor threshold for HC->Run conversion.
--
-- The slot lock is enforced by a dedicated `self_organize_slot_claims`
-- table (one row per held slot) rather than partial unique indexes on
-- `headcounts` / `runs`, so the lock can survive the HC->Run transition
-- via a single-row UPDATE that flips which FK is non-null.
--
-- Idle auto-cancel is purely lazy — there is no background task. A stale
-- HC is swept when (a) someone clicks "Start a run" for the same slot,
-- (b) the listing message is rebuilt (stale rows are filtered out), or
-- (c) the bot restarts (orphan sweep reconciles dangling claims).
-- ---------------------------------------------------------------------------

-- ---- tiers: per-tier configuration -----------------------------------------
ALTER TABLE tiers
    ADD COLUMN enable_self_organization              BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN self_organize_channel_id              BIGINT,
    ADD COLUMN self_organize_button_message_id       BIGINT,
    ADD COLUMN self_organize_listing_message_id      BIGINT,
    ADD COLUMN self_organize_idle_minutes            INTEGER NOT NULL DEFAULT 15
        CHECK (self_organize_idle_minutes > 0),
    ADD COLUMN self_organize_cancel_cooldown_seconds INTEGER NOT NULL DEFAULT 300
        CHECK (self_organize_cancel_cooldown_seconds >= 0),
    ADD COLUMN self_organize_min_reactors            INTEGER NOT NULL DEFAULT 2
        CHECK (self_organize_min_reactors >= 0);

-- ---- headcounts / runs: mark which rows came in via the self-organize
-- flow, used to filter the per-user cap and to choose the right code path
-- in cancel/convert/end handlers. -------------------------------------------
ALTER TABLE headcounts
    ADD COLUMN is_self_organized BOOLEAN NOT NULL DEFAULT FALSE;

ALTER TABLE runs
    ADD COLUMN is_self_organized BOOLEAN NOT NULL DEFAULT FALSE;

-- ---- self_organize_slot_claims: per-(tier, dungeon) lock -------------------
--
-- Exactly one row per held slot. `headcount_id` and `run_id` are both
-- nullable but the CHECK forces at least one of them to be set, so the
-- HC->Run swap is a single UPDATE that flips which FK is non-null without
-- ever briefly violating the invariant.
--
-- ON DELETE NO ACTION on the FKs forces all releases to go through
-- transactional `claim_release_by_*` helpers that delete the claim row
-- *before* the HC/Run row, so we never end up in (NULL, NULL) which would
-- violate the CHECK constraint.
--
-- `is_self_organized` is denormalized from the linked HC/Run so the
-- per-user cap (idx_so_one_per_user) can be enforced as a partial unique
-- index with a single-table query.
CREATE TABLE self_organize_slot_claims (
    guild_id            BIGINT      NOT NULL REFERENCES guilds(guild_id)        ON DELETE CASCADE,
    tier_id             INT         NOT NULL REFERENCES tiers(id)               ON DELETE CASCADE,
    dungeon_template_id INT         NOT NULL REFERENCES dungeon_templates(id)   ON DELETE CASCADE,
    leader_user_id      BIGINT      NOT NULL,
    is_self_organized   BOOLEAN     NOT NULL,
    headcount_id        INT         REFERENCES headcounts(id) ON DELETE NO ACTION,
    run_id              INT         REFERENCES runs(id)       ON DELETE NO ACTION,
    acquired_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (guild_id, tier_id, dungeon_template_id),
    UNIQUE (headcount_id),
    UNIQUE (run_id),
    CHECK ((headcount_id IS NOT NULL) OR (run_id IS NOT NULL))
);

CREATE INDEX idx_so_slot_claims_guild ON self_organize_slot_claims(guild_id);

-- One live self-organized raid per user, guild-wide. Staff-led HCs in
-- self-organize-enabled tiers also write claim rows (so they obey the
-- slot lock) but do NOT count against the user cap.
CREATE UNIQUE INDEX idx_so_one_per_user
    ON self_organize_slot_claims (guild_id, leader_user_id)
    WHERE is_self_organized = TRUE;

-- ---- self_organize_user_cooldowns: post-cancel cooldown --------------------
--
-- Set when a user cancels their *own* self-organized HC. Checked on the
-- next "Start a run" click. Lazy-pruned: `cooldown_active` deletes any
-- expired row it observes, so no background task is needed.
CREATE TABLE self_organize_user_cooldowns (
    guild_id   BIGINT      NOT NULL REFERENCES guilds(guild_id) ON DELETE CASCADE,
    tier_id    INT         NOT NULL REFERENCES tiers(id)        ON DELETE CASCADE,
    user_id    BIGINT      NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (guild_id, tier_id, user_id)
);
