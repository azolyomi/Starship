-- Simplify the loot-tier threshold from per-guild-per-dungeon to
-- per-guild-only. Rendering is "what's the floor tier a guild wants to
-- see?"; having a per-dungeon knob was more granular than any server
-- actually needs, and the per-dungeon table complicated the rendering
-- pipeline for no real gain.
--
-- Existing rows are not preserved — the dev DB has none set to
-- non-default values, and operators can re-run `/config threshold` if
-- they need something other than 'white'.

ALTER TABLE guilds
    ADD COLUMN loot_tier_threshold TEXT NOT NULL DEFAULT 'white'
    REFERENCES bag_tiers(name);

DROP TABLE guild_loot_tier_threshold;
