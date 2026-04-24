-- Shinies are rarer than white-bag drops and drop from the same encounters,
-- so they fit as the tier above white in the same lookup table. No real
-- "shiny bag" icon exists on RealmEye — the unicode ✨ fallback in
-- default_emoji is what the renderer will use unless a custom bag_shiny
-- application emoji is uploaded later.

INSERT INTO bag_tiers (name, sort_order, default_emoji) VALUES
    ('shiny', 8, '✨');
