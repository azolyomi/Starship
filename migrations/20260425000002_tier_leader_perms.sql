-- ---------------------------------------------------------------------------
-- Replace the dead `tier_roles` table with tier-scoped grants in
-- `permissions`. Before this migration, the setup wizard collected "access
-- roles" per tier into `tier_roles`, but no runtime code consulted that
-- table — so users who configured tier roles still hit "no permission" when
-- starting headcounts. The intent was always "these roles can lead raids in
-- this tier", which is exactly what a tier-scoped permission row encodes.
--
-- For each existing (tier_id, role_id) we insert one row per leader action,
-- scoped to that tier. The action set mirrors what express setup grants the
-- "Raid Leader" role guild-wide.
-- ---------------------------------------------------------------------------

INSERT INTO permissions (guild_id, role_id, action, tier_id, dungeon_template_id)
SELECT t.guild_id, tr.role_id, action_name, tr.tier_id, NULL
FROM tier_roles tr
JOIN tiers t ON t.id = tr.tier_id
CROSS JOIN UNNEST(ARRAY[
    'StartHeadcount',
    'ConvertHeadcount',
    'CancelHeadcount',
    'StartRun',
    'EndRun',
    'ManageRuns',
    'CreateVcRaid'
]) AS action_name
ON CONFLICT DO NOTHING;

DROP TABLE tier_roles;
