-- R4: item tracking moves from DB-backed buttons to native Discord reactions.
--
-- With "we trust users", the per-user tally tables are no longer needed:
-- the bot attaches the required emojis to the message at creation and
-- users click them directly. The emojis themselves are the tally.
--
-- Also drops the legacy channel columns made redundant by R3's
-- `tiers.runs_channel_id`. `guilds.notification_channel_id` goes with
-- them — the notifications feature was removed in R3 in favour of the
-- per-dungeon `notification_role_id` + `/pingroles` flow.
--
-- Preflight: startup refuses to run through this migration if any active
-- headcounts/runs exist, unless `STARSHIP_ALLOW_MIGRATION=1`. The SQL
-- itself is unconditional — the gate is in the Rust bootstrap.

DROP TABLE IF EXISTS headcount_reactions;
DROP TABLE IF EXISTS run_participants;

ALTER TABLE tiers  DROP COLUMN IF EXISTS raid_channel_id;
ALTER TABLE tiers  DROP COLUMN IF EXISTS headcount_channel_id;
ALTER TABLE guilds DROP COLUMN IF EXISTS notification_channel_id;
