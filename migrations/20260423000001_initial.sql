-- Initial schema for starship.
--
-- Design notes:
--   * All Discord snowflakes are stored as BIGINT (signed 64-bit). Discord IDs
--     fit in 63 bits so this is safe; using unsigned types would need extra
--     casting and PG has no native u64.
--   * `guild_id` on dungeon_templates is NULLABLE — NULL rows are the global
--     built-in defaults; non-NULL rows are per-guild overrides/additions.
--   * The ON DELETE policy is `CASCADE` only where a child record is
--     meaningless without the parent (e.g. tier_dungeons). For headcounts and
--     runs we keep history with ON DELETE RESTRICT so we don't accidentally
--     destroy audit data.
--   * Timestamps default to NOW() and use TIMESTAMPTZ to avoid DST bugs.

-- ---------------------------------------------------------------------------
-- guilds: top-level server configuration.
-- ---------------------------------------------------------------------------
CREATE TABLE guilds (
    guild_id                 BIGINT       PRIMARY KEY,
    log_channel_id           BIGINT,
    notification_channel_id  BIGINT,
    superadmin_user_id       BIGINT,
    setup_complete           BOOLEAN      NOT NULL DEFAULT FALSE,
    created_at               TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    updated_at               TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

-- ---------------------------------------------------------------------------
-- tiers: isolated server sections (Main / Veterans / Elite).
-- ---------------------------------------------------------------------------
CREATE TABLE tiers (
    id                    SERIAL       PRIMARY KEY,
    guild_id              BIGINT       NOT NULL REFERENCES guilds(guild_id) ON DELETE CASCADE,
    name                  TEXT         NOT NULL,
    description           TEXT,
    raid_channel_id       BIGINT,
    headcount_channel_id  BIGINT,
    created_at            TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    UNIQUE (guild_id, name)
);

CREATE INDEX idx_tiers_guild ON tiers(guild_id);

-- ---------------------------------------------------------------------------
-- tier_roles: which Discord roles grant access to which tier.
-- ---------------------------------------------------------------------------
CREATE TABLE tier_roles (
    tier_id  INT     NOT NULL REFERENCES tiers(id) ON DELETE CASCADE,
    role_id  BIGINT  NOT NULL,
    PRIMARY KEY (tier_id, role_id)
);

-- ---------------------------------------------------------------------------
-- dungeon_templates: dungeon definitions.
-- NULL guild_id = global default template. Non-NULL = per-guild override or
-- custom addition.
-- ---------------------------------------------------------------------------
CREATE TABLE dungeon_templates (
    id                    SERIAL       PRIMARY KEY,
    guild_id              BIGINT       REFERENCES guilds(guild_id) ON DELETE CASCADE,
    name                  TEXT         NOT NULL,
    display_name          TEXT         NOT NULL,
    emoji                 TEXT,
    color                 INT,
    message_title         TEXT,
    message_description   TEXT,
    thumbnail_url         TEXT,
    image_url             TEXT,
    requires_vc           BOOLEAN      NOT NULL DEFAULT FALSE,
    notification_role_id  BIGINT,
    showcase_emoji        TEXT[]       NOT NULL DEFAULT '{}',
    created_at            TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

-- One "name" per guild — and only one NULL-guild (global) entry per name.
CREATE UNIQUE INDEX idx_dungeon_templates_guild_name
    ON dungeon_templates(COALESCE(guild_id, 0), name);

-- ---------------------------------------------------------------------------
-- dungeon_reactions: required items / reactions per dungeon template.
-- ---------------------------------------------------------------------------
CREATE TABLE dungeon_reactions (
    id                      SERIAL   PRIMARY KEY,
    dungeon_template_id     INT      NOT NULL REFERENCES dungeon_templates(id) ON DELETE CASCADE,
    name                    TEXT     NOT NULL,
    display_name            TEXT     NOT NULL,
    emoji                   TEXT     NOT NULL,
    num_required            INT      NOT NULL CHECK (num_required >= 0),
    requires_confirmation   BOOLEAN  NOT NULL DEFAULT FALSE,
    sort_order              INT      NOT NULL DEFAULT 0,
    UNIQUE (dungeon_template_id, name)
);

CREATE INDEX idx_dungeon_reactions_template ON dungeon_reactions(dungeon_template_id);

-- ---------------------------------------------------------------------------
-- tier_dungeons: which dungeons are available in which tier.
-- ---------------------------------------------------------------------------
CREATE TABLE tier_dungeons (
    tier_id              INT  NOT NULL REFERENCES tiers(id) ON DELETE CASCADE,
    dungeon_template_id  INT  NOT NULL REFERENCES dungeon_templates(id) ON DELETE CASCADE,
    PRIMARY KEY (tier_id, dungeon_template_id)
);

-- ---------------------------------------------------------------------------
-- permissions: per-action permission grants.
-- A row means: members of role_id in guild_id may perform <action>, optionally
-- scoped to a specific tier and/or dungeon template.
-- ---------------------------------------------------------------------------
CREATE TABLE permissions (
    id                   SERIAL  PRIMARY KEY,
    guild_id             BIGINT  NOT NULL REFERENCES guilds(guild_id) ON DELETE CASCADE,
    role_id              BIGINT  NOT NULL,
    action               TEXT    NOT NULL,
    tier_id              INT     REFERENCES tiers(id) ON DELETE CASCADE,
    dungeon_template_id  INT     REFERENCES dungeon_templates(id) ON DELETE CASCADE
);

-- Uniqueness: coalesce NULLs to 0 so "all tiers / all dungeons" collapses to a
-- single row per (guild, role, action).
CREATE UNIQUE INDEX idx_permissions_unique
    ON permissions(
        guild_id,
        role_id,
        action,
        COALESCE(tier_id, 0),
        COALESCE(dungeon_template_id, 0)
    );
CREATE INDEX idx_permissions_lookup ON permissions(guild_id, action);

-- ---------------------------------------------------------------------------
-- emoji_servers: Discord guilds dedicated to hosting custom emoji for us.
-- ---------------------------------------------------------------------------
CREATE TABLE emoji_servers (
    guild_id     BIGINT  PRIMARY KEY,
    description  TEXT
);

-- ---------------------------------------------------------------------------
-- bot_emoji: logical name -> Discord emoji mapping.
-- Snowflake IDs are globally valid, so pg_dump/restore carries mappings across
-- machines.
-- ---------------------------------------------------------------------------
CREATE TABLE bot_emoji (
    id                SERIAL  PRIMARY KEY,
    logical_name      TEXT    NOT NULL UNIQUE,
    discord_emoji_id  BIGINT  NOT NULL,
    source_guild_id   BIGINT  REFERENCES emoji_servers(guild_id) ON DELETE SET NULL,
    category          TEXT,
    realmeye_url      TEXT
);

CREATE INDEX idx_bot_emoji_category ON bot_emoji(category);

-- ---------------------------------------------------------------------------
-- headcounts: active headcount lifecycle rows.
-- ---------------------------------------------------------------------------
CREATE TABLE headcounts (
    id                    SERIAL       PRIMARY KEY,
    guild_id              BIGINT       NOT NULL REFERENCES guilds(guild_id) ON DELETE CASCADE,
    tier_id               INT          NOT NULL REFERENCES tiers(id)        ON DELETE RESTRICT,
    dungeon_template_id   INT          NOT NULL REFERENCES dungeon_templates(id) ON DELETE RESTRICT,
    channel_id            BIGINT       NOT NULL,
    message_id            BIGINT       NOT NULL,
    leader_user_id        BIGINT       NOT NULL,
    status                TEXT         NOT NULL DEFAULT 'active'
                                       CHECK (status IN ('active','converted','expired','cancelled')),
    created_at            TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    updated_at            TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_headcounts_status ON headcounts(status) WHERE status = 'active';
CREATE INDEX idx_headcounts_message ON headcounts(message_id);

-- ---------------------------------------------------------------------------
-- headcount_reactions: who reacted to which item on a headcount.
-- ---------------------------------------------------------------------------
CREATE TABLE headcount_reactions (
    id                    SERIAL       PRIMARY KEY,
    headcount_id          INT          NOT NULL REFERENCES headcounts(id) ON DELETE CASCADE,
    dungeon_reaction_id   INT          NOT NULL REFERENCES dungeon_reactions(id) ON DELETE RESTRICT,
    user_id               BIGINT       NOT NULL,
    confirmed             BOOLEAN      NOT NULL DEFAULT FALSE,
    confirmed_at          TIMESTAMPTZ,
    UNIQUE (headcount_id, dungeon_reaction_id, user_id)
);

CREATE INDEX idx_headcount_reactions_headcount ON headcount_reactions(headcount_id);

-- ---------------------------------------------------------------------------
-- runs: active + historical runs (may originate from a headcount).
-- ---------------------------------------------------------------------------
CREATE TABLE runs (
    id                    SERIAL       PRIMARY KEY,
    guild_id              BIGINT       NOT NULL REFERENCES guilds(guild_id) ON DELETE CASCADE,
    tier_id               INT          NOT NULL REFERENCES tiers(id)        ON DELETE RESTRICT,
    dungeon_template_id   INT          NOT NULL REFERENCES dungeon_templates(id) ON DELETE RESTRICT,
    headcount_id          INT          REFERENCES headcounts(id) ON DELETE SET NULL,
    channel_id            BIGINT       NOT NULL,
    message_id            BIGINT       NOT NULL,
    leader_user_id        BIGINT       NOT NULL,
    location              TEXT,
    party                 TEXT,
    voice_channel_id      BIGINT,
    is_vc_raid            BOOLEAN      NOT NULL DEFAULT FALSE,
    status                TEXT         NOT NULL DEFAULT 'active'
                                       CHECK (status IN ('active','ended')),
    created_at            TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    ended_at              TIMESTAMPTZ
);

CREATE INDEX idx_runs_status ON runs(status) WHERE status = 'active';
CREATE INDEX idx_runs_message ON runs(message_id);
CREATE INDEX idx_runs_guild ON runs(guild_id);

-- ---------------------------------------------------------------------------
-- run_participants: members on a run and what they brought.
-- dungeon_reaction_id = NULL means "joined but no declared item".
-- ---------------------------------------------------------------------------
CREATE TABLE run_participants (
    id                    SERIAL       PRIMARY KEY,
    run_id                INT          NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    user_id               BIGINT       NOT NULL,
    dungeon_reaction_id   INT          REFERENCES dungeon_reactions(id) ON DELETE RESTRICT,
    confirmed             BOOLEAN      NOT NULL DEFAULT FALSE,
    joined_at             TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

-- Uniqueness: coalesce NULL reaction to 0 so a given user can only have one
-- "no item" row per run, plus one row per distinct item.
CREATE UNIQUE INDEX idx_run_participants_unique
    ON run_participants(run_id, user_id, COALESCE(dungeon_reaction_id, 0));
CREATE INDEX idx_run_participants_run ON run_participants(run_id);

-- ---------------------------------------------------------------------------
-- Auto-update `updated_at` on guilds / headcounts when rows change.
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION touch_updated_at()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at := NOW();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_guilds_touch
    BEFORE UPDATE ON guilds
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

CREATE TRIGGER trg_headcounts_touch
    BEFORE UPDATE ON headcounts
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
