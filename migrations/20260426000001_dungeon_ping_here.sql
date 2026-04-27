-- Per-dungeon @here toggle.
--
-- The bot has always pinged @here on every headcount/run start in
-- addition to the bound notification role. This adds a per-dungeon
-- override on the existing dungeon_notification_roles table so admins
-- can opt specific dungeons out (e.g. low-stakes runs that shouldn't
-- notify the whole channel) without touching the role binding.
--
-- Default is TRUE so the migration is behaviour-preserving for every
-- existing row. role_id is also relaxed to nullable: a row can exist
-- purely to remember a `ping_here = false` override even when no role
-- is bound, and `set_notification_role(.., None)` now NULLs the role
-- (rather than deleting the row) when a ping_here override is present.

ALTER TABLE dungeon_notification_roles
    ADD COLUMN ping_here BOOLEAN NOT NULL DEFAULT TRUE;

ALTER TABLE dungeon_notification_roles
    ALTER COLUMN role_id DROP NOT NULL;
