-- ---------------------------------------------------------------------------
-- Unify headcount protections across all tiers and retire "self-organize"
-- as a behavioural concept. The remaining tier-level toggle controls only
-- the sticky-button + listing UI; slot lock, per-user cap, post-cancel
-- cooldown, and min-reactor convert gate now apply to every headcount.
--
-- The slot lock collapses from a separate `self_organize_slot_claims`
-- table to a plain UNIQUE index on `headcounts(guild_id, tier_id,
-- dungeon_template_id)`. Headcounts already follow the "row exists iff
-- live" convention (see 20260424000007_raid_lifecycle_cleanup.sql), so a
-- non-partial unique index is the whole story. Runs no longer hold a
-- slot — once a run starts, the next group can begin forming.
-- ---------------------------------------------------------------------------

-- ---- Drop the dedicated slot-claim table. Pre-launch, no live data.
DROP INDEX IF EXISTS idx_so_slot_claims_guild;
DROP INDEX IF EXISTS idx_so_one_per_user;
DROP TABLE IF EXISTS self_organize_slot_claims;

-- ---- The `is_self_organized` flag has no remaining readers: protections
-- are universal, the listing renderer treats every HC the same.
ALTER TABLE headcounts DROP COLUMN is_self_organized;
ALTER TABLE runs       DROP COLUMN is_self_organized;

-- ---- Rename tier columns. The toggle now means "show the start-run UI in
-- this tier's runs channel", nothing more. The cooldown / idle / min-reactor
-- knobs are universal HC settings, so they shed the `self_organize_` prefix.
ALTER TABLE tiers RENAME COLUMN enable_self_organization              TO enable_start_run_ui;
ALTER TABLE tiers RENAME COLUMN self_organize_channel_id              TO start_run_ui_channel_id;
ALTER TABLE tiers RENAME COLUMN self_organize_button_message_id       TO start_run_ui_button_message_id;
ALTER TABLE tiers RENAME COLUMN self_organize_listing_message_id      TO start_run_ui_listing_message_id;
ALTER TABLE tiers RENAME COLUMN self_organize_idle_minutes            TO hc_idle_minutes;
ALTER TABLE tiers RENAME COLUMN self_organize_cancel_cooldown_seconds TO hc_cancel_cooldown_seconds;
ALTER TABLE tiers RENAME COLUMN self_organize_min_reactors            TO hc_min_reactors;

-- ---- Rename the post-cancel cooldown table to match the new universal
-- semantic. Same shape, same primary key.
ALTER TABLE self_organize_user_cooldowns RENAME TO hc_user_cooldowns;

-- ---- The slot-lock invariant: at most one active HC per (guild, tier,
-- dungeon). Concurrent /hc clicks race on the INSERT — Postgres surfaces
-- the unique violation as SQLSTATE 23505, which the service layer
-- translates into a "slot in use" outcome.
CREATE UNIQUE INDEX idx_hc_one_active_per_slot
    ON headcounts (guild_id, tier_id, dungeon_template_id);
