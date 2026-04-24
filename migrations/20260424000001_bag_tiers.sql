-- Bag tiers: RotMG loot bag rarity classification.
-- Each drop in bot_emoji is tagged with the bag it drops from so the
-- rendering layer can group drops by tier and filter by a per-guild
-- threshold.
--
-- Modeled as a lookup table rather than an ENUM so the sort_order can
-- drive "at or above tier X" comparisons via joins, and adding a new
-- tier later (if Deca introduces one) is an INSERT rather than an
-- ALTER TYPE.

CREATE TABLE bag_tiers (
    name           TEXT  PRIMARY KEY,
    sort_order     INT   NOT NULL UNIQUE,
    default_emoji  TEXT  NOT NULL
);

INSERT INTO bag_tiers (name, sort_order, default_emoji) VALUES
    ('brown',  0, '🟫'),
    ('pink',   1, '🌸'),
    ('purple', 2, '🟪'),
    ('cyan',   3, '🟦'),
    ('blue',   4, '🔵'),
    ('orange', 5, '🟧'),
    ('red',    6, '🟥'),
    ('white',  7, '⬜');

ALTER TABLE bot_emoji
    ADD COLUMN bag_tier TEXT REFERENCES bag_tiers(name) ON DELETE RESTRICT;

CREATE INDEX idx_bot_emoji_bag_tier ON bot_emoji(bag_tier);
