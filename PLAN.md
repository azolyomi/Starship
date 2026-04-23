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

Emoji images live on Discord as **Application Emojis** (owned by the bot app itself, up to 2000) -- the DB only stores the mapping (logical_name -> discord_emoji_id). Application-emoji IDs are globally valid across every guild the bot is in and do not require `USE_EXTERNAL_EMOJIS`, so a `pg_dump`/`pg_restore` plus the same bot token is all that's needed to move everything between dev and prod. (Using a different bot token in prod means re-running `starship sync-wiki` to re-upload emojis to the new app.)

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

Primary path: **Discord Application Emojis** (owned by the bot app, up to 2000),
managed via `POST/GET/PATCH/DELETE /applications/{app_id}/emojis` with bot-token
auth. No dedicated emoji-hosting guild is required, and app emojis do not need
`USE_EXTERNAL_EMOJIS` to render. The `emoji_servers` table is intentionally
omitted for now -- RotMG realistically needs ~300-500 emojis, well under the
2000 cap. `source_guild_id` is kept on `bot_emoji` as a nullable escape hatch so
overflow into guild-hosted emojis can be added later without a schema break.

> Note: Phase 2 originally shipped with a guild-hosted design (`emoji_servers`
> table, FK `source_guild_id`, scraper uploading to `POST /guilds/{id}/emojis`
> gated on `EMOJI_GUILD_ID`). Phase 2.5 (below) unwinds that. This section
> describes the target shape.

```sql
-- Logical emoji name -> Discord emoji ID mapping.
-- NULL source_guild_id = application emoji (the normal case).
-- Non-NULL = emoji hosted in a guild (reserved for future overflow;
-- emoji_servers table + upload path can be added if the 2000 cap is ever hit).
bot_emoji (
    id SERIAL PRIMARY KEY,
    logical_name TEXT UNIQUE NOT NULL,   -- e.g., "helm_rune", "divinity", "shatters_key"
    discord_emoji_id BIGINT NOT NULL,    -- emoji snowflake (app emoji or guild emoji)
    name_on_discord TEXT NOT NULL,       -- registered name on Discord (for <:name:id> rendering)
    animated BOOLEAN NOT NULL DEFAULT FALSE,
    source_guild_id BIGINT,              -- NULL = application emoji; set if hosted in a guild
    category TEXT,                       -- "key", "portal", "drop", "ui"
    realmeye_url TEXT,                   -- source image URL for re-scraping
    uploaded_at TIMESTAMPTZ DEFAULT NOW()
)
```

Why these columns:
- `name_on_discord` -- the render syntax is `<:name:id>`, and the Discord-side
  name has tighter rules than our logical name (e.g. `helm_rune` -> `helmrune`).
  Storing both keeps the code-side logical name stable while the Discord-side
  name can be edited independently.
- `animated` -- lets the renderer choose `<:...>` vs `<a:...>`.
- `source_guild_id` -- one-column hedge. Zero runtime cost today; if overflow is
  ever needed, re-introduce `emoji_servers`, populate this column, and the
  rendering path is unchanged (both app and guild emojis use `<:name:id>`).
- `uploaded_at` -- supports diff/resync logic in `sync-wiki`.

### Step 2.5 -- Application Emoji Migration (cleanup of shipped Phase 2)

Phase 2 (commit `f482e9a`) shipped a guild-hosted emoji design: a dedicated
`emoji_servers` table, an FK `source_guild_id` on `bot_emoji`, and a scraper
that uploads to `POST /guilds/{id}/emojis` gated on `EMOJI_GUILD_ID`. This
requires the bot to be a member of a dedicated emoji-hosting guild and caps it
at 50 emojis/guild (250 Nitro-boosted). Discord Application Emojis (~2000 per
app, no guild required, no `USE_EXTERNAL_EMOJIS` needed) replace this entirely.

**Concrete changes**:

1. **Migration** -- add a new `migrations/YYYYMMDDHHMMSS_application_emojis.sql`
   (do not edit `20260423000001_initial.sql` in place; preserve the chain).
   The migration must:
   - `DROP TABLE emoji_servers;`
   - `ALTER TABLE bot_emoji DROP CONSTRAINT bot_emoji_source_guild_id_fkey;`
     (the FK to `emoji_servers`; the column itself stays, now nullable with no
     FK, as the future-overflow hedge).
   - `ALTER TABLE bot_emoji ADD COLUMN name_on_discord TEXT NOT NULL DEFAULT '';`
     then backfill from existing rows if any, then drop the default.
   - `ALTER TABLE bot_emoji ADD COLUMN animated BOOLEAN NOT NULL DEFAULT FALSE;`
   - `ALTER TABLE bot_emoji ADD COLUMN uploaded_at TIMESTAMPTZ NOT NULL DEFAULT NOW();`

2. **`src/db/models.rs` (`BotEmoji` struct at L60)** -- add fields
   `name_on_discord: String`, `animated: bool`, `uploaded_at: DateTime<Utc>`
   matching the new schema. `source_guild_id` stays as `Option<i64>`.

3. **`src/db/emoji.rs`** -- delete `register_emoji_server` (L61-78) and
   `list_emoji_servers` (L80-86). Update `upsert`, `get_by_logical_name`, and
   `get_all` column lists to include `name_on_discord`, `animated`,
   `uploaded_at`. Add a new `ApplicationEmojiClient` (or put it in a new
   `src/services/emoji_api.rs`) wrapping:
   - `GET    /applications/{app_id}/emojis`
   - `POST   /applications/{app_id}/emojis`
   - `PATCH  /applications/{app_id}/emojis/{emoji_id}`
   - `DELETE /applications/{app_id}/emojis/{emoji_id}`

   Auth header `Bot {DISCORD_TOKEN}`. `app_id` is already in
   `Config::discord_application_id`.

4. **`src/cli/sync_wiki.rs`** -- this is the biggest change:
   - Delete the `emoji_guild_id == 0` skip branch (L82-87) and the
     `register_emoji_server` call (L100). App emoji upload has no opt-out.
   - Replace the upload helper at L395-412 (currently `POST /guilds/{id}/emojis`)
     with a call to `ApplicationEmojiClient::create(name, image_bytes)`.
   - At the top of the run, call `ApplicationEmojiClient::list()` and build a
     `HashMap<String, i64>` of existing-app-emoji name -> id. Skip uploads
     whose `name_on_discord` is already present. This reconciles manual
     Developer-Portal edits.
   - Pass `source_guild_id: None` to `db::emoji::upsert` (app emojis), and set
     `name_on_discord` and `animated` on upsert.
   - Leave a `// TODO: overflow path` comment at the upload site marking where
     a guild-emoji fallback would plug in if the 2000 cap is ever hit. Do not
     implement it.

5. **`src/config.rs`** -- remove `emoji_guild_id` (L10, L27, L51-56, L64).

6. **`.env.example`** -- remove the `EMOJI_GUILD_ID` block (L36-39).

7. **Rendering** -- whichever embed/label helper renders emojis (to be built in
   Phases 4-5) must read `name_on_discord`, `animated`, and `discord_emoji_id`
   from `bot_emoji` and emit `<:name:id>` or `<a:name:id>`. The logical name
   used in code (e.g. `"helm_rune"`) stays the join key.

8. **Progress log** -- append a new `### 2026-04-23 — Phase 2.5 complete`
   entry to the `## Progress` section when the above lands.

**Verification**:

- `sqlx migrate run` on a fresh DB applies both `20260423000001_initial.sql`
  and the new migration cleanly.
- `cargo build` passes.
- `grep -r "emoji_server\|EMOJI_GUILD\|register_emoji_server" src/` returns no
  matches.
- `cargo run -- sync-wiki` against a test bot populates the application-emoji
  list (verify in Developer Portal -> Emojis tab, or via
  `curl -H "Authorization: Bot $DISCORD_TOKEN" https://discord.com/api/v10/applications/$APP_ID/emojis`).
- Rendering smoke test: post a test headcount in a guild where the bot's
  `@everyone` role does **not** have `USE_EXTERNAL_EMOJIS`; emojis must still
  render.
- `pg_dump` -> fresh DB -> `pg_restore`: the bot still renders emojis correctly
  without re-running `sync-wiki`, as long as the same bot token is used.

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

**Step 5**: Upload to the bot's application emojis via `POST /applications/{app_id}/emojis` (bot-token auth), register `(logical_name, discord_emoji_id, name_on_discord, animated)` in `bot_emoji` with `source_guild_id = NULL`. Diff against the existing list from `GET /applications/{app_id}/emojis` to skip unchanged emojis and reconcile manual edits made via the Developer Portal. Leave a `// TODO: overflow path` marker at the upload site for a future guild-emoji fallback -- do not implement it now.

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

### Phase 2.5: Application Emoji Migration
10.5. Replace the guild-hosted emoji design from Phase 2 with Discord Application Emojis. See the "Step 2.5 -- Application Emoji Migration" section above for concrete file-by-file changes (new migration, `ApplicationEmojiClient`, `sync-wiki` rewrite, config/env cleanup).

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

### 2026-04-23 — Phase 2 complete

Landed:
- `src/templates/mod.rs` + `src/templates/dungeons.rs` — 8 built-in dungeon
  templates as compile-time `&'static` data: O3, Void, Shatters, Lost Halls,
  Cultist, Nest, Fungal Cavern, Crystal Cavern. Each has interest + key/rune
  reactions with sort order and confirm flags.
- `src/db/dungeon.rs` — DB query layer: `seed_builtins` (upsert-on-conflict
  global templates at startup), `list_for_guild` (DISTINCT ON with guild
  override precedence), `get_by_name`, `get_reactions`, `insert_guild_template`,
  `update_guild_template`, `delete_guild_template`, `upsert_global_template`,
  `upsert_reaction` (used by sync-wiki).
- `src/db/emoji.rs` — `upsert`, `get_by_logical_name`, `get_all`,
  `register_emoji_server`, `list_emoji_servers`.
- `src/db/mod.rs` — exposes dungeon and emoji submodules.
- `src/commands/dungeon.rs` — `/dungeon list`, `/dungeon create`, `/dungeon edit`,
  `/dungeon delete` slash commands. Color parsed from hex string. Handles
  uniqueness conflict on create with a user-friendly message.
- `src/commands/mod.rs` — adds `/dungeon` to the command list.
- `src/cli/sync_wiki.rs` — full RealmEye scraper: fetches dungeon index table,
  scrapes per-dungeon drop pages, downloads+resizes images (128×128 PNG),
  uploads to Discord emoji server via REST, upserts `bot_emoji` and
  `dungeon_templates`. Selector constants grouped at top for easy maintenance.
  Respects Discord rate limit headers. Skips emoji upload if `EMOJI_GUILD_ID`
  not set.
- `src/config.rs` — added `emoji_guild_id: Option<u64>`.
- `.env.example` — documented `EMOJI_GUILD_ID`.
- `src/main.rs` — `mod templates` added; calls `db::dungeon::seed_builtins`
  after migrations on every startup.
- `Cargo.toml` — added `base64 = "0.22"`.
- **`cargo build` passes** (14 dead_code warnings, no errors — all for
  functions used in Phases 3+).

### 2026-04-23 — Phase 2.5 complete

Landed:
- `migrations/20260423000002_application_emojis.sql` — drops `emoji_servers`,
  drops the FK on `bot_emoji.source_guild_id`, adds `name_on_discord TEXT`,
  `animated BOOLEAN`, `uploaded_at TIMESTAMPTZ` to `bot_emoji`.
- `src/db/models.rs` — `BotEmoji` struct updated with `name_on_discord`,
  `animated`, `uploaded_at`.
- `src/db/emoji.rs` — removed `register_emoji_server` / `list_emoji_servers`;
  updated `upsert`, `get_by_logical_name`, `get_all` column lists; added
  `ApplicationEmojiClient` with `list()` and `create()` methods targeting
  `POST/GET /applications/{app_id}/emojis` (bot-token auth).
- `src/cli/sync_wiki.rs` — rewrote emoji upload path to use
  `ApplicationEmojiClient`; calls `list()` at startup to build a diff map,
  skips emojis already registered; passes `source_guild_id: None`; added
  `discord_name()` helper for Discord-safe name coercion; added
  `// TODO: overflow path` marker at the upload site.
- `src/config.rs` — removed `emoji_guild_id` field and loader.
- `.env.example` — removed `EMOJI_GUILD_ID` block.
- **`cargo build` passes** (13 dead_code warnings for future phases, no errors).
- `grep -r "emoji_server\|EMOJI_GUILD\|register_emoji_server" src/` → no matches.

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
