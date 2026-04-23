-- Phase 2.5: replace guild-hosted emoji design with Discord Application Emojis.
--
-- Application Emojis are owned by the bot application itself (up to 2000).
-- They require no dedicated emoji-hosting guild and no USE_EXTERNAL_EMOJIS permission.
-- The source_guild_id column is kept as a nullable escape hatch for a future guild-emoji
-- overflow path, but the FK to emoji_servers is removed since the table is gone.

ALTER TABLE bot_emoji DROP CONSTRAINT bot_emoji_source_guild_id_fkey;

DROP TABLE emoji_servers;

ALTER TABLE bot_emoji ADD COLUMN name_on_discord TEXT        NOT NULL DEFAULT '';
ALTER TABLE bot_emoji ADD COLUMN animated        BOOLEAN     NOT NULL DEFAULT FALSE;
ALTER TABLE bot_emoji ADD COLUMN uploaded_at     TIMESTAMPTZ NOT NULL DEFAULT NOW();
