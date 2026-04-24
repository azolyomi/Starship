-- R3: consolidate tiers to a single runs channel.
--
-- Legacy `raid_channel_id` and `headcount_channel_id` (and
-- `guilds.notification_channel_id`) survive this migration; R4 drops them.
-- During R3 the write path dual-writes `runs_channel_id` into the legacy
-- columns so anything still reading the old names sees the same value.

ALTER TABLE tiers
    ADD COLUMN runs_channel_id BIGINT;

-- Backfill from whichever legacy column was set. Prefer raid_channel_id —
-- that's the channel where the run message ended up going post-conversion,
-- which is what `runs` now means.
UPDATE tiers
   SET runs_channel_id = COALESCE(raid_channel_id, headcount_channel_id)
 WHERE runs_channel_id IS NULL;
