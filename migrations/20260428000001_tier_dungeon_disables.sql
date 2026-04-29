-- Soft-disable mechanism for global dungeon templates.
--
-- Background: globals are now implicitly visible to every tier in every
-- guild. Previously /setup wrote a tier_dungeons row for every global
-- so the start-run picker would see it; that meant new globals seeded
-- after /setup (e.g. void_remnant added via dungeon_overrides.json) were
-- invisible until an admin ran /tier add-dungeon by hand. Flipping to
-- "implicit by default" fixes that, but admins still need a way to hide
-- a specific global from a specific tier.
--
-- This table stores those opt-outs. The picker query becomes:
--   (globals NOT IN tier_dungeon_disables for this tier)
--   ∪ (guild-specific templates IN tier_dungeons for this tier)
--
-- Guild-specific templates are NOT stored here; they're attached/removed
-- via tier_dungeons (the existing explicit-attachment table).
--
-- Migration of existing data: clean slate. Pre-existing /tier
-- remove-dungeon choices on globals are NOT preserved as disable rows.
-- Backfilling them would re-hide newly-added globals that admins never
-- actively removed (e.g. Void Remnant), defeating the purpose. In
-- practice almost no guilds have manually trimmed globals.

CREATE TABLE tier_dungeon_disables (
    tier_id              INT NOT NULL REFERENCES tiers(id) ON DELETE CASCADE,
    dungeon_template_id  INT NOT NULL REFERENCES dungeon_templates(id) ON DELETE CASCADE,
    PRIMARY KEY (tier_id, dungeon_template_id)
);
