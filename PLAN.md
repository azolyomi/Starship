# Starship Bot - Full Rewrite Plan

## Context

Starship is a Discord bot for facilitating "raids" (organized dungeon runs) in Realm of the Mad God. The existing Python/discord.py/MongoDB implementation is incomplete and uses outdated patterns. This is a ground-up rewrite in Rust targeting reliability, low latency, full configurability, and crash resilience. The existing repo will be fully replaced.

## Development & Deployment

**Dev environment**: WSL2 on Windows. Postgres runs locally inside WSL. The bot is built and tested entirely within WSL -- identical to the production target.

**Production**: Dedicated Linux VPS. Clone repo, run `setup.sh`, optionally restore a DB dump from dev, run `deploy.sh`.

**Scripts**:
- `setup.sh` -- installs rustup, PostgreSQL, sqlx-cli. Creates the database, runs migrations, copies `.env.example` to `.env` for editing. Accepts `--restore <dump_file>` to load an existing DB dump (preserves emoji mappings, templates, guild configs from dev).
- `deploy.sh` -- production-only: installs systemd service + watchdog, enables log rotation, starts the bot.

**Data migration** (dev -> prod):
```bash
# On WSL (dev)
pg_dump starship > starship.sql

# Copy to VPS
scp starship.sql vps:~/

# On VPS
./setup.sh --restore starship.sql
./deploy.sh
```

Emoji images live on Discord (uploaded to emoji servers) -- the DB only stores the mapping (logical_name -> discord_emoji_id). These snowflake IDs are globally valid, so a `pg_dump`/`pg_restore` is all that's needed to move everything.

## Tech Stack

| Layer | Choice | Rationale |
|-------|--------|-----------|
| Language | Rust | Lowest latency, memory-safe, tokio async |
| Discord | serenity + poise | Stable, slash commands, components, actively maintained |
| Voice | songbird | serenity-native VC management |
| Database | PostgreSQL (local) | Free, runs on same machine (WSL in dev, VPS in prod), sqlx compile-time checks |
| Migrations | sqlx-cli | Built-in migration tooling |
| Process | systemd | Auto-restart, watchdog |

## Interaction Model

Buttons + modals everywhere. No reactions. Discord API v10.

- **Headcount**: labeled buttons with emoji per required item
- **Confirmation**: `confirm: true` items pop a modal on click; success updates the headcount embed and shows on leader's control panel
- **Control panel**: "Control Panel" button on run message, leader-only. Each click = fresh ephemeral message with current state + action buttons. Text inputs (location, party) via modals.
- **Toggle**: users can remove confirmations by clicking again; updates propagate everywhere
- **Notification roles**: persistent message with buttons to self-assign dungeon notification roles

## custom_id Routing (stateless, survives restarts)

```
hc:<id>:react:<reaction_id>     -- headcount reaction
hc:<id>:start                   -- convert to run
hc:<id>:cancel                  -- cancel headcount
run:<id>:cp                     -- open control panel (leader only)
run:<id>:join                   -- join run
run:<id>:leave                  -- leave run
run:<id>:loc                    -- modal: set location
run:<id>:party                  -- modal: set party
run:<id>:transfer               -- select menu: new leader
run:<id>:end                    -- end run
run:<id>:confirm:<reaction_id>  -- confirm item bringing
notify:<guild>:<template_id>    -- toggle notification role
```

## Data Model

### Core Tables

```sql
-- Server configuration
guilds (
    guild_id BIGINT PRIMARY KEY,  -- Discord snowflake
    log_channel_id BIGINT,
    notification_channel_id BIGINT,  -- where role-selection message lives
    superadmin_user_id BIGINT,
    setup_complete BOOLEAN DEFAULT FALSE,
    created_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ
)

-- Isolated server sections (Main, Veterans, Elite)
tiers (
    id SERIAL PRIMARY KEY,
    guild_id BIGINT REFERENCES guilds,
    name TEXT NOT NULL,
    description TEXT,
    raid_channel_id BIGINT,        -- where run messages go
    headcount_channel_id BIGINT,   -- where headcount messages go
    created_at TIMESTAMPTZ
)

-- Discord roles that grant tier access
tier_roles (
    tier_id INT REFERENCES tiers,
    role_id BIGINT,
    PRIMARY KEY (tier_id, role_id)
)

-- Dungeon definitions (built-in defaults + per-guild overrides)
dungeon_templates (
    id SERIAL PRIMARY KEY,
    guild_id BIGINT REFERENCES guilds NULL,  -- NULL = global default
    name TEXT NOT NULL,                       -- e.g., "oryx3", "fullskip_void"
    display_name TEXT NOT NULL,               -- e.g., "Oryx's Sanctuary"
    emoji TEXT,                               -- logical emoji name
    color INT,                                -- embed color
    message_title TEXT,
    message_description TEXT,
    thumbnail_url TEXT,
    image_url TEXT,
    requires_vc BOOLEAN DEFAULT FALSE,
    notification_role_id BIGINT,              -- per-template ping role
    showcase_emoji TEXT[],                    -- list of logical emoji names for rare drops
    created_at TIMESTAMPTZ
)

-- Required items/reactions per dungeon template
dungeon_reactions (
    id SERIAL PRIMARY KEY,
    dungeon_template_id INT REFERENCES dungeon_templates,
    name TEXT NOT NULL,            -- e.g., "interest", "helmet_rune"
    display_name TEXT NOT NULL,    -- e.g., "Interested", "Helmet Rune"
    emoji TEXT NOT NULL,           -- logical emoji name
    num_required INT NOT NULL,
    requires_confirmation BOOLEAN DEFAULT FALSE,
    sort_order INT DEFAULT 0
)

-- Which dungeons are available in which tier
tier_dungeons (
    tier_id INT REFERENCES tiers,
    dungeon_template_id INT REFERENCES dungeon_templates,
    PRIMARY KEY (tier_id, dungeon_template_id)
)

-- Per-action permission grants
permissions (
    id SERIAL PRIMARY KEY,
    guild_id BIGINT REFERENCES guilds,
    role_id BIGINT NOT NULL,                           -- Discord role
    action TEXT NOT NULL,                               -- enum string
    tier_id INT REFERENCES tiers NULL,                 -- NULL = all tiers
    dungeon_template_id INT REFERENCES dungeon_templates NULL,  -- NULL = all dungeons
    UNIQUE (guild_id, role_id, action, tier_id, dungeon_template_id)
)

-- Permission actions enum:
-- StartHeadcount, ConvertHeadcount, CancelHeadcount,
-- StartRun, EndRun, ManageRuns, CreateVcRaid,
-- ConfigureGuild, ManageTiers, ManagePermissions, ManageDungeons
```

### Emoji Management

```sql
-- Servers dedicated to hosting custom emoji
emoji_servers (
    guild_id BIGINT PRIMARY KEY,
    description TEXT
)

-- Logical emoji name -> Discord emoji ID mapping
bot_emoji (
    id SERIAL PRIMARY KEY,
    logical_name TEXT UNIQUE NOT NULL,  -- e.g., "helm_rune", "divinity", "shatters_key"
    discord_emoji_id BIGINT NOT NULL,
    source_guild_id BIGINT REFERENCES emoji_servers,
    category TEXT,                      -- "key", "portal", "drop", "ui"
    realmeye_url TEXT                   -- source image URL for re-scraping
)
```

### Raid Lifecycle

```sql
-- Active headcounts
headcounts (
    id SERIAL PRIMARY KEY,
    guild_id BIGINT REFERENCES guilds,
    tier_id INT REFERENCES tiers,
    dungeon_template_id INT REFERENCES dungeon_templates,
    channel_id BIGINT NOT NULL,
    message_id BIGINT NOT NULL,
    leader_user_id BIGINT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',  -- active, converted, expired, cancelled
    created_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ
)

-- Who reacted to headcount items
headcount_reactions (
    id SERIAL PRIMARY KEY,
    headcount_id INT REFERENCES headcounts,
    dungeon_reaction_id INT REFERENCES dungeon_reactions,
    user_id BIGINT NOT NULL,
    confirmed BOOLEAN DEFAULT FALSE,
    confirmed_at TIMESTAMPTZ,
    UNIQUE (headcount_id, dungeon_reaction_id, user_id)
)

-- Active runs (may originate from headcount)
runs (
    id SERIAL PRIMARY KEY,
    guild_id BIGINT REFERENCES guilds,
    tier_id INT REFERENCES tiers,
    dungeon_template_id INT REFERENCES dungeon_templates,
    headcount_id INT REFERENCES headcounts NULL,
    channel_id BIGINT NOT NULL,
    message_id BIGINT NOT NULL,
    leader_user_id BIGINT NOT NULL,
    location TEXT,
    party TEXT,
    voice_channel_id BIGINT,        -- temp VC, NULL if vcless
    is_vc_raid BOOLEAN DEFAULT FALSE,
    status TEXT NOT NULL DEFAULT 'active',  -- active, ended
    created_at TIMESTAMPTZ,
    ended_at TIMESTAMPTZ
)

-- Run participants and what they brought
run_participants (
    id SERIAL PRIMARY KEY,
    run_id INT REFERENCES runs,
    user_id BIGINT NOT NULL,
    dungeon_reaction_id INT REFERENCES dungeon_reactions NULL,
    confirmed BOOLEAN DEFAULT FALSE,
    joined_at TIMESTAMPTZ,
    UNIQUE (run_id, user_id, dungeon_reaction_id)
)
```

## Crash Recovery

On startup:
1. Query all `active` headcounts and runs from Postgres
2. For each, verify the Discord message still exists via API
3. Orphaned messages -> mark `cancelled` / `ended`
4. Orphaned voice channels -> delete
5. All future interactions route statelessly via custom_id -> DB lookup

## RealmEye Wiki Scraper (`starship sync-wiki`)

CLI subcommand for bootstrapping and refreshing emoji + dungeon data.

**Step 1**: Scrape `realmeye.com/wiki/dungeons`
- Parse table: dungeon name, portal image URL, key image URL, dungeon wiki link

**Step 2**: For each dungeon, scrape its wiki page
- Parse "Drops of Interest" table: item name, image URL, item wiki link

**Step 3**: For each drop item, scrape its individual wiki page
- Check for "Assigned to White Bag" text -> tag as showcase-eligible
- Grab sprite image URL

**Step 4**: Download all images (portals, keys, white bag sprites)
- Resize to Discord emoji constraints (128x128, max 256KB)

**Step 5**: Upload to emoji server(s) via Discord API, register in `bot_emoji` table

**Step 6**: Auto-generate default `dungeon_templates` and `dungeon_reactions` from scraped data
- Each dungeon gets a template with: display name, portal emoji, key reaction (if key exists), interest reaction, showcase_emoji (white bag drops)

Re-running picks up new dungeons/items (diff against existing DB entries).

## Project Structure

```
starship/
├── Cargo.toml
├── .env.example
├── setup.sh             -- dev + prod setup (rust, postgres, migrations)
├── deploy.sh            -- prod-only (systemd, watchdog, log rotation)
├── starship.service     -- systemd unit file
├── migrations/
│   ├── 20260423_001_initial.sql
│   └── ...
├── src/
│   ├── main.rs              -- entry, bot setup, crash recovery
│   ├── config.rs            -- env/config loading
│   ├── db/
│   │   ├── mod.rs
│   │   ├── models.rs        -- structs (Guild, Tier, DungeonTemplate, Run, etc.)
│   │   ├── guild.rs
│   │   ├── tier.rs
│   │   ├── dungeon.rs
│   │   ├── headcount.rs
│   │   ├── run.rs
│   │   ├── permission.rs
│   │   └── emoji.rs
│   ├── commands/
│   │   ├── mod.rs
│   │   ├── headcount.rs     -- /headcount <dungeon> [tier]
│   │   ├── run.rs           -- /run <dungeon> [tier] (direct, no headcount)
│   │   ├── config.rs        -- /config (guild settings)
│   │   ├── tier.rs          -- /tier create/delete/list/assign
│   │   ├── dungeon.rs       -- /dungeon create/edit/delete/list
│   │   ├── permission.rs    -- /permission grant/revoke/list
│   │   ├── setup.rs         -- /setup (onboarding)
│   │   └── notifications.rs -- /notifications (post role-selection message)
│   ├── handlers/
│   │   ├── mod.rs
│   │   ├── component.rs     -- button/select custom_id routing
│   │   └── modal.rs         -- modal submission routing
│   ├── services/
│   │   ├── mod.rs
│   │   ├── raid.rs          -- headcount/run lifecycle (create, convert, end, chain)
│   │   ├── permission.rs    -- permission checking with superadmin bypass
│   │   ├── guild.rs         -- guild setup/teardown
│   │   └── voice.rs         -- temp VC create/cleanup
│   ├── embeds/
│   │   ├── mod.rs
│   │   ├── headcount.rs     -- headcount embed + component builders
│   │   └── run.rs           -- run embed + component builders
│   ├── templates/
│   │   └── dungeons.rs      -- built-in dungeon definitions (fallback defaults)
│   └── cli/
│       └── sync_wiki.rs     -- RealmEye scraper CLI subcommand
```

## Slash Commands

| Command | Description | Permission |
|---------|-------------|------------|
| `/setup` | Onboarding flow for new guild | Discord admin |
| `/headcount <dungeon> [tier]` | Start a headcount | StartHeadcount |
| `/run <dungeon> [tier]` | Start a run directly | StartRun |
| `/config log_channel <channel>` | Set log channel | ConfigureGuild |
| `/config notification_channel <channel>` | Set notification channel | ConfigureGuild |
| `/tier create/delete/list/edit` | Manage tiers | ManageTiers |
| `/dungeon create/edit/delete/list` | Manage dungeon templates | ManageDungeons |
| `/permission grant/revoke/list` | Manage permissions | ManagePermissions |
| `/notifications` | Post role-selection message | ConfigureGuild |

## Build Order

### Phase 1: Foundation
1. Initialize Rust project with Cargo, set up dependencies
2. `setup.sh` script (rustup, postgres, sqlx-cli, DB creation, migrations, `--restore` flag)
3. Database schema + migrations
4. Config loading (.env)
5. Bot skeleton (serenity + poise, connects to Discord + Postgres)
6. Guild model + `/setup` command

### Phase 2: Dungeon Templates & Emoji
7. Built-in dungeon template definitions
8. Emoji server registration + `bot_emoji` table
9. RealmEye scraper CLI (`starship sync-wiki`)
10. `/dungeon` CRUD commands

### Phase 3: Permissions & Tiers
11. Permission service with superadmin bypass
12. `/permission` commands
13. Tier CRUD + `/tier` commands
14. Notification role management + `/notifications`

### Phase 4: Headcount Lifecycle
15. `/headcount` command - creates embed + buttons
16. Headcount reaction handler (click -> confirm modal -> DB + embed update)
17. Toggle/remove confirmation
18. Convert headcount to run (`hc:<id>:start`)
19. Cancel headcount

### Phase 5: Run Lifecycle
20. Run message creation (from headcount or direct `/run`)
21. Control panel (ephemeral, leader-only, on-demand)
22. Location/party modals
23. Ownership transfer
24. End run
25. Chained run support (run continues until leader ends it)

### Phase 6: Voice Channel Management
26. Temp VC creation for VC raids
27. VC join enforcement
28. VC cleanup on run end

### Phase 7: Reliability
29. Crash recovery on startup
30. `deploy.sh` + systemd service file + watchdog
31. Logging + error handling

### Phase 8: Polish
32. `/config` commands
33. Embed styling (showcase emoji, colors, thumbnails)
34. Edge cases (leader leaves server, channel deleted, etc.)

## Verification Extensibility (future)

Architecture already supports this:
- Add `verified_users` table: discord_user_id -> realmeye_ign, verified_at
- Verification flow: user sets a code in their RealmEye description, bot fetches `realmeye.com/player/<ign>`, checks code
- Permission service gets an optional `requires_verification` flag per action/tier
- No structural changes needed

## Testing / Verification

- Run `cargo build` to verify compilation at each phase
- Run `sqlx migrate run` to verify schema
- Test bot in a dedicated Discord test server
- Manual testing of each interaction flow (headcount -> confirm -> convert -> run -> control panel -> end)
- Test crash recovery by killing the bot mid-run and restarting
- Test permission denials for unauthorized users

## Progress

Append-only log of what has landed, so a fresh Claude context can pick up
from here without re-reading transcripts.

### 2026-04-23 — Phase 1 complete

Landed:
- Legacy Python `src/`, `requirements.txt`, `README.md` removed.
- `.gitignore` rewritten for Rust (keeps `.env` out of git).
- `Cargo.toml` — serenity 0.12 + poise 0.6 + songbird 0.4 + sqlx 0.8 + tokio
  + tracing + reqwest + scraper + image + clap.
- `.env.example` — documents every env var the bot reads.
- `setup.sh` — idempotent: installs build deps (cmake, libopus-dev), rustup,
  postgresql, sqlx-cli; creates `starship` DB user with a generated password;
  writes `DATABASE_URL` into `.env` (mode 600); runs migrations; supports
  `--restore <dump>`.
- `migrations/20260423000001_initial.sql` — full schema per PLAN.
- `CLAUDE.md` — project rules: breakpoint cadence, credential management.
- `src/main.rs` — entry point: CLI (bot / sync-wiki), tracing init, bot setup
  via poise + serenity; guild-local command registration when
  `DISCORD_TEST_GUILD_ID` is set.
- `src/config.rs` — `Config` struct loaded once from `.env` via dotenvy;
  `Debug` impl masks secrets (first 4 chars + length).
- `src/db/mod.rs` + `src/db/models.rs` — `PgPool` factory; typed structs for
  every table (`Guild`, `Tier`, `DungeonTemplate`, `DungeonReaction`,
  `BotEmoji`, `Headcount`, `Run`).
- `src/commands/mod.rs` + `src/commands/setup.rs` — `/setup` command stub.
- `src/handlers/`, `src/embeds/`, `src/services/`, `src/cli/` — all modules
  scaffolded as stubs (compile, no logic yet).
- **`cargo build` passes** (7 dead_code warnings, no errors). Rust 1.95.0,
  sqlx 0.8, serenity 0.12, poise 0.6.

### Credentials still needed from the user

Collected into `.env` when we're ready to boot:
- `DISCORD_TOKEN` — from https://discord.com/developers/applications → Bot
  tab → Reset Token. Enable the **Message Content** and **Server Members**
  privileged intents while there.
- `DISCORD_APPLICATION_ID` — same application → General Information.
- `DISCORD_TEST_GUILD_ID` *(optional but strongly recommended for dev)* —
  right-click the test server in Discord with Developer Mode on → Copy
  Server ID.

`DATABASE_URL` is populated automatically by `setup.sh`; do not set it
by hand.
