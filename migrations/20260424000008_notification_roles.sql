-- Pull per-dungeon notification role bindings out of dungeon_templates.
--
-- The old design stored `notification_role_id` directly on the template
-- row and — because a global row (`guild_id IS NULL`) can't carry a
-- per-guild role ID — `set_notification_role` cloned the global into a
-- guild-specific row whenever a role was bound. The clone was a scalar
-- snapshot: reactions, display name, etc. were copied to the clone, and
-- any subsequent override edit on the global (e.g. adding a key reaction
-- via `data/dungeon_overrides.json`) never propagated. `list_for_guild`'s
-- `DISTINCT ON (name)` then kept handing back the stale clone.
--
-- This migration decouples the role binding from the template:
--   * `dungeon_notification_roles` stores (guild_id, dungeon_name, role_id).
--   * `set_notification_role` writes here instead of cloning templates.
--   * Globals remain authoritative for reactions + display; future
--     override edits apply cleanly on the next boot.
--
-- Data handling:
--   1. Copy every existing non-null notification_role_id into the new
--      table keyed by (guild_id, dungeon_name).
--   2. Delete guild-specific template rows that only exist because of
--      a role clone — i.e. whose scalar fields still match the current
--      global with the same name. Custom templates created via
--      `/dungeon create` or `/dungeon edit` stay untouched.
--   3. Drop the `notification_role_id` column from dungeon_templates.

CREATE TABLE dungeon_notification_roles (
    guild_id       BIGINT  NOT NULL REFERENCES guilds(guild_id) ON DELETE CASCADE,
    dungeon_name   TEXT    NOT NULL,
    role_id        BIGINT  NOT NULL,
    PRIMARY KEY (guild_id, dungeon_name)
);

CREATE INDEX idx_dungeon_notification_roles_guild
    ON dungeon_notification_roles(guild_id);

-- Copy existing bindings. Only guild-specific rows can have a role set;
-- a stray role on a `guild_id IS NULL` row would be a schema violation,
-- so we ignore those.
INSERT INTO dungeon_notification_roles (guild_id, dungeon_name, role_id)
SELECT guild_id, name, notification_role_id
FROM dungeon_templates
WHERE guild_id IS NOT NULL
  AND notification_role_id IS NOT NULL
ON CONFLICT (guild_id, dungeon_name) DO NOTHING;

-- Drop clone-only guild-specific template rows. A clone is a guild row
-- whose scalar fields still match the corresponding global verbatim.
-- `IS NOT DISTINCT FROM` handles the NULL-equals-NULL cases for emoji /
-- color / message_* fields. `showcase_emoji` and `thumbnail_url` are
-- deliberately NOT in the match: the seeder rewrites those on every
-- boot so a clone's snapshot often legitimately diverges from the live
-- global even when the clone was never user-edited.
--
-- Guard against destroying adjacent data: skip any row that is still
-- referenced by an active headcount or run (deletion would fail under
-- ON DELETE RESTRICT), and skip rows that have scoped permissions or
-- tier memberships — those CASCADE-delete, and we don't want to throw
-- away a permission grant just because we're tidying up role bindings.
DELETE FROM dungeon_templates t
WHERE t.guild_id IS NOT NULL
  AND EXISTS (
    SELECT 1 FROM dungeon_templates g
    WHERE g.guild_id IS NULL
      AND g.name = t.name
      AND g.display_name = t.display_name
      AND g.emoji                 IS NOT DISTINCT FROM t.emoji
      AND g.color                 IS NOT DISTINCT FROM t.color
      AND g.message_title         IS NOT DISTINCT FROM t.message_title
      AND g.message_description   IS NOT DISTINCT FROM t.message_description
      AND g.requires_vc           =  t.requires_vc
  )
  AND NOT EXISTS (SELECT 1 FROM headcounts    h WHERE h.dungeon_template_id = t.id)
  AND NOT EXISTS (SELECT 1 FROM runs          r WHERE r.dungeon_template_id = t.id)
  AND NOT EXISTS (SELECT 1 FROM permissions   p WHERE p.dungeon_template_id = t.id)
  AND NOT EXISTS (SELECT 1 FROM tier_dungeons td WHERE td.dungeon_template_id = t.id);

ALTER TABLE dungeon_templates DROP COLUMN notification_role_id;
