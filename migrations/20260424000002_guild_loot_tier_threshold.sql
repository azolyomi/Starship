-- R2: per-guild, per-dungeon bag-tier threshold.
--
-- Embeds render drops grouped by the bag they come from (brown → white).
-- This table lets each guild hide noisy low-tier drops on a per-dungeon
-- basis: the renderer only shows tiers whose sort_order is ≥ the stored
-- threshold. Absence of a row = default 'white' (strictest: only white
-- bags are shown).
--
-- tier_name FKs into bag_tiers so invalid tier strings can't be written.

CREATE TABLE guild_loot_tier_threshold (
    guild_id            BIGINT NOT NULL REFERENCES guilds(guild_id) ON DELETE CASCADE,
    dungeon_template_id INT    NOT NULL REFERENCES dungeon_templates(id) ON DELETE CASCADE,
    tier_name           TEXT   NOT NULL REFERENCES bag_tiers(name) ON DELETE RESTRICT,
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (guild_id, dungeon_template_id)
);
