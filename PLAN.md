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

> **Note:** the SQL below is the original design sketch. The authoritative
> schema lives in `migrations/`. Several columns / tables here have been
> changed or removed since: `guilds.notification_channel_id` and
> `tiers.{headcount,raid}_channel_id` are gone (R3/R4); `headcount_reactions`
> and `run_participants` are gone (R4 — Discord reactions carry signups now);
> `headcounts.status`, `runs.status`, `runs.ended_at`, `runs.headcount_id`
> are gone (migration `…000007_raid_lifecycle_cleanup.sql` — terminal
> transitions DELETE the row instead). `guilds.loot_tier_threshold` was
> added (Phase 6.5) and the per-dungeon threshold table was dropped.

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

Migration `20260424000007_raid_lifecycle_cleanup.sql` collapsed the lifecycle
to a queue: a `headcounts` / `runs` row exists iff the raid is live, and
terminal transitions (`Start Run`, `Cancel`, `End`) `DELETE` the row. That
removes the original "scan for `status='active'` and reconcile" pass — there
is no terminal state to mark.

What still works for free:
- Component buttons route statelessly via `custom_id` → DB lookup, so a
  restart mid-raid resumes cleanly as long as the DB row + Discord message
  are both still present.

What's still missing (residual orphan sweep, scheduled for after Phase 7):
1. **Stale DB rows.** A run row whose Discord message was deleted while the
   bot was offline lives forever. On startup, fetch each row's `message_id`;
   if the channel/message is gone, `DELETE` the row.
2. **Orphan VCs.** `services::voice::create_temp_vc` writes
   `runs.voice_channel_id` before the bot might crash. If a run's VC exists
   but the run row does not, delete the VC. (Bot restart between
   `End run` and the explicit VC delete is the realistic failure mode.)
3. **Re-attach reactions.** `attach_signup_reactions` runs once on raid
   creation. If the bot crashed mid-attach, the message is missing one or
   more reactions; on startup, diff present-vs-required for every active
   raid's message.

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

> **Note:** the tree below is the original sketch. Current layout differs in
> a few places: `commands/notifications.rs` was deleted in R3 and replaced
> by `commands/pingroles.rs`; `services/reactions.rs` was added in R4;
> `templates/mod.rs` (the dump+overrides pipeline) replaced
> `templates/dungeons.rs` in Phase 6.5; and `cli/upload_emoji.rs` was added
> in R4. The intended final shape includes a `Dockerfile` +
> `docker-compose.yml` (Phase 7b) alongside the existing
> `setup.sh` / `deploy.sh` / `starship.service`.

```
starship/
├── Cargo.toml
├── .env.example
├── setup.sh             -- bare-metal dev setup (rust, postgres, migrations)
├── Dockerfile           -- multi-stage build → slim runtime image (Phase 7b)
├── docker-compose.yml   -- postgres + bot stack (Phase 7b)
├── deploy.sh            -- VPS-side: git pull + compose up (Phase 7b)
├── starship.service     -- tiny systemd unit that starts compose at boot
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

Phases 1–6.5 are complete; see the Progress log below for what landed in each.
The build order is preserved here for orientation.

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

### Rework 2026-04 (R1–R4)

Triggered after Phase 5 by a round of user feedback. Not new features —
corrections to `sync-wiki`, headcount/run UX, and `/setup` that change the
project's operating model. Voice (original Phase 6) is deferred until R4
lands. See `.claude/plans/current-problems-1-sync-sharded-kernighan.md` for
the full design.

**R1 — Scraper correctness + emoji purge**
- Fix `slug_from_display` to strip `'` / `\u{2019}` (so "Oryx's Sanctuary"
  → `oryxs_sanctuary`, not `oryx_s_sanctuary`).
- Scope `scrape_dungeon_list` to the Regular Dungeons section; skip
  Special Event + Other Dungeons; hardcode-exclude Court of Oryx,
  Oryx's Castle/Chamber, Wine Cellar.
- Rewrite `extract_drops_section` with a multi-heading match so Snake
  Pit and similar dungeons land their drops.
- New `bag_tiers` lookup table (brown→white, sort_order 0–7) + `bag_tier`
  column on `bot_emoji`.
- New `scrape_item_page` visits each drop's wiki page, extracts the
  "Loot Bag" row (color word → `bag_tier`; img src → bag icon URL).
- First time a bag-tier icon is seen in a run, upload as `bag_<tier>`
  under `bot_emoji` (category `ui`).
- New `--purge` flag on `sync-wiki`: DELETE all application emojis and
  TRUNCATE `bot_emoji`, gated on `dialoguer::Confirm`. One-shot op to
  retire the old bad-slug emoji set.
- Rename the `oryx_s_sanctuary` built-in template to `oryxs_sanctuary`.
- Auto-migrate old-format keys in `data/curation.json` on load.

**R2 — Bag-tier rendering + per-dungeon threshold**
- New `guild_loot_tier_threshold(guild_id, dungeon_template_id, tier_name)`
  table. Absence = default `white` (strictest).
- New `src/db/loot.rs` module: `list_bag_tiers`, `resolve_bag_emoji` (global,
  falls back to `bag_tiers.default_emoji`), `get_threshold`, `set_threshold`.
- Headcount + run embed helpers render drops grouped by bag tier: one
  field per tier ≥ threshold, prefixed by the bag emoji, value = joined
  drop emojis. Tier footer removed from every embed variant.
- New `/config threshold <dungeon> <tier>` command (ConfigureGuild).
- Scraper stops filtering `showcase_emoji` to white-only; writes every
  drop, and grouping-by-tier in the renderer does the filtering.

**R3 — Setup simplification + `/pingroles`**
- `tiers.runs_channel_id` column; backfill from existing raid/headcount
  channels. Phase R3 dual-writes old + new columns; R4 drops the legacy
  columns.
- Quick `/setup` creates one `<tier>-runs` channel (not two) and renames
  the log channel default to `🚀starship-log` with a fallback on emoji
  rejection.
- Remove the notifications-channel concept entirely from `/setup`.
- Delete `src/commands/notifications.rs`.
- New `src/commands/pingroles.rs`:
  - `/pingroles` (anyone) → paginated ephemeral: 10 dungeons/page,
    multi-select to toggle opt-in for each dungeon, diffs desired vs.
    current role membership and applies add/remove with 3× retry.
  - `/pingroles set/unset/create <dungeon> [role]` admin subcommands
    (ConfigureGuild-gated) that write `dungeon_templates.notification_role_id`.
- `start_headcount` now pings `notification_role_id` just like
  `start_run` already does.

**R4 — Reactions replace buttons + multi-item unlocks + cleanup**
- Drop `headcount_reactions` and `run_participants` tables (no longer
  tracked — we trust users).
- Drop legacy `tiers.raid_channel_id`, `tiers.headcount_channel_id`,
  `guilds.notification_channel_id` columns.
- New `src/services/reactions.rs` with `attach_reactions` helper:
  retry 5× on 429/5xx, loud-ping organizer on final failure.
- `start_headcount` + `start_run` call `attach_reactions` with
  `:white_check_mark:` + each required-item emoji after the message posts.
- Delete all button-based item flow: `hc:*:react:*`, `hc:*:confirm:*`,
  `run:*:join|leave|confirm*` handlers + their embed components.
- Templates: O3 gets `wine_cellar_incantation` + three existing runes
  (5 reactions total with ✅); Void gets `lost_halls_key` +
  `vial_of_the_void`; Cultist drops to just `lost_halls_key`. All
  `requires_confirmation` flags set to false.
- New `upload-emoji` CLI helper for one-off manual emoji uploads
  (needed for `wine_cellar_incantation` if the scraper doesn't find it).
- Organizer gating via new `require_organizer_or_manage_runs` helper:
  leader OR `ManageRuns` permission can use start/end/cancel/control-panel
  buttons; anyone else gets an ephemeral denial.
- Drop per-reaction roster fields + the `👥 Joined` field from run
  embeds. `build_ended` produces a minimal greyed embed — no roster.
- Preflight: on startup, if active headcounts/runs exist, log loud error
  and exit unless `STARSHIP_ALLOW_MIGRATION=1` is set.

### Phase 6: Voice Channel Management
26. Temp VC creation for VC raids
27. ~~VC join enforcement~~ — deferred (users trust each other; revisit later)
28. VC cleanup on run end

### Phase 6.5: Dungeon config refactor
- Linearise the dungeon-template data flow (kill the two-source collision
  between hardcoded builtins and the scraper).
- Move loot-tier threshold from per-dungeon to per-guild.
- Delete the curation module that overrides.json now subsumes.

### Phase 7: Reliability
- **7a. Logging + error handling sweep.** Configure `tracing-subscriber`
  for stdout (Docker `docker logs` + systemd `journalctl` both consume it
  cleanly), opt-in JSON output via `RUST_LOG_FORMAT=json`, structured
  fields instead of formatted strings, lifecycle spans for raids and
  interactions, top-level error reporter that captures command + caller
  + guild + error chain. Audit `?` bubble points that should
  warn-and-continue instead of aborting a flow.
- **7b. Containerised deploy.** Multi-stage `Dockerfile` (Rust build →
  slim runtime image). `docker-compose.yml` with `postgres:16` + bot
  service, `.env` injected via `env_file:`. `deploy.sh` on the VPS is a
  thin wrapper: `git pull && docker compose up -d`. A small systemd unit
  starts Compose at boot (no per-process supervisor — Compose's
  `restart: unless-stopped` is the watchdog). `setup.sh` stays for
  bare-metal dev.
- **7c. Orphan sweep on startup.** See "Crash Recovery" above. Delete
  `runs` rows whose Discord message no longer exists; delete temp VCs
  whose run row is gone; re-attach missing signup reactions on still-live
  raids.

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

### 2026-04-23 — Phase 3 complete

Landed:
- `src/db/guild.rs` — `get`, `upsert`, `set_superadmin`, `set_log_channel`,
  `set_notification_channel`. Minimal layer; used by permission service today,
  rest used when `/setup`/`/config` are fully implemented.
- `src/db/permission.rs` — `grant`, `revoke`, `list_for_guild`, `check`.
  `check` uses a dynamic query (not a macro) to pass `role_ids: &[i64]` via
  `ANY($2)`.
- `src/db/tier.rs` — `create`, `list`, `get_by_id`, `get_by_name`, `update`,
  `delete`, `add_role`, `remove_role`, `list_roles`, `add_dungeon`,
  `remove_dungeon`, `list_dungeons`. Full CRUD + junction tables.
- `src/db/mod.rs` — exposes `guild`, `permission`, `tier` modules.
- `src/db/models.rs` — added `Permission` struct.
- `src/services/permission.rs` — `Action` enum (11 variants), `require`,
  `require_str`, `require_discord_admin`. Superadmin bypass via
  `guild.superadmin_user_id`; role-based fallback via `db::permission::check`.
  `ALL_ACTIONS` constant for autocomplete.
- `src/commands/permission.rs` — `/permission grant/revoke/list`.
  Requires `ManagePermissions`. Autocomplete for action name. Tier/dungeon
  scope args resolve by name.
- `src/commands/tier.rs` — `/tier create/delete/list/edit/add-role/remove-role/
  add-dungeon/remove-dungeon`. All require `ManageTiers`. Autocomplete for
  tier and dungeon names.
- `src/commands/notifications.rs` — `/notifications` stub (permission-gated
  on `ConfigureGuild`; full embed + button build is Phase 4).
- `src/commands/mod.rs` — all three new command roots registered.
- Applied `20260423000002_application_emojis.sql` migration (was pending from
  Phase 2.5 commit).
- **`cargo build` passes** (23 dead_code warnings for Phase 4+ functions, no
  errors).

### 2026-04-23 — Phase 3 fixups

- `src/services/permission.rs` — added Discord "Manage Server" bypass as
  second tier (after superadmin, before role check) so server admins can
  always run management commands without needing a DB grant.
- `src/main.rs` — added `ensure_setup` framework `command_check`: any
  command except `/setup` returns an ephemeral "Run `/setup` first" message
  if no guild row exists, preventing FK errors on first use.
- `CLAUDE.md` — added "Secret files" rule: never read `.env` files.

### 2026-04-23 — `/setup` wizard (Phase 1 item 6 upgraded from stub)

Real first-run experience so Phase 4 can land on top of a properly
configured guild.

- `src/commands/setup.rs` — full dashboard-style wizard built on ephemeral
  messages + component interactions:
  - **Dashboard** shows checklist (First tier ✅/⬜, Superadmin, Log channel,
    Notif channel) with row of section buttons + Finish/Close. Finish is
    disabled until at least one tier has a headcount channel.
  - **First tier** section: ChannelSelect (headcount, required), ChannelSelect
    (raid, optional), RoleSelect (access roles, multi). On create, names the
    tier "Main" and automatically attaches every globally-available dungeon so
    `/headcount` works out of the box. On edit, syncs roles via
    add/remove diff.
  - **Superadmin** section: UserSelect pre-populated to current superadmin,
    with [Use me] / [Clear] / [← Back] buttons.
  - **Log channel** / **Notif channel**: shared `channel_section_view`
    helper, text-channel-only selects, [Clear] / [← Back].
  - All writes are immediate except the first-tier section, which drafts
    in-memory and commits atomically on [Create tier] / [Save changes].
    Timeouts (10 min idle) edit the ephemeral to a retry-friendly message.
  - Re-running `/setup` shows current state with every select
    pre-populated, doubling as a config surface.
- `src/db/guild.rs` — `set_superadmin` now takes `Option<i64>` to support
  clearing; added `mark_setup_complete(pool, guild_id, complete)`.
- **`cargo build` passes** (16 dead_code warnings for Phase 4+ scaffolding,
  no errors).

Phase 4 prerequisites are now met: a fresh server can `/setup` → Finish →
immediately run `/headcount <dungeon>` in its Main tier.

### 2026-04-23 — `/setup` follow-ups

- `src/services/permission.rs` — hardcoded `GLOBAL_SUPERADMIN_USER_ID =
  942_320_785_287_184_464` as an operator override. Checked first in
  `require_discord_admin`, `require`, and `require_str` so the operator
  always passes every permission gate in every guild.
- `src/commands/setup.rs` — **Create default channels** button in the
  first-tier section. When at least one of headcount/raid is unset, clicking
  it finds-or-creates a "Raids" category and two text channels
  (`{tier-slug}-headcount`, `{tier-slug}-raid-room`) under it, then
  populates the draft selects. Idempotent — re-clicking picks up existing
  channels with the expected names rather than duplicating.

### 2026-04-23 — Phase 4 complete

Landed:
- `src/db/headcount.rs` — DB layer: `create`, `set_message_id`, `get`, `set_status`,
  `list_active`, `reaction_counts`, `get_user_reaction`, `add_reaction`, `remove_reaction`.
- `src/db/models.rs` — added `HeadcountReaction` struct; added `#[derive(Clone)]` to `Tier`.
- `src/db/dungeon.rs` — added `get_by_id` (used by component handlers).
- `src/db/emoji.rs` — added `get_all_as_map` (builds a `HashMap<logical_name, BotEmoji>`).
- `src/db/mod.rs` — exposes `headcount` module.
- `src/embeds/headcount.rs` — full embed builder: `emoji_str`, `emoji_rt`, `build`
  (embed + reaction/leader buttons), `build_closed` (grey=cancelled / green=converted).
- `src/commands/headcount.rs` — `/headcount <dungeon> [tier]` command: autocomplete for
  both args, permission check (`StartHeadcount`), tier auto-resolve when only one exists,
  delegates to `services::raid::start_headcount`.
- `src/commands/mod.rs` — `/headcount` registered.
- `src/services/raid.rs` — `start_headcount`: creates DB row, posts embed to headcount
  channel, updates `message_id` after send.
- `src/handlers/component.rs` — stateless `custom_id` routing for `hc:*` buttons:
  `hc:<id>:react:<rid>` (toggle or open confirmation modal), `hc:<id>:start` (leader only,
  marks converted + closes embed), `hc:<id>:cancel` (leader only, marks cancelled +
  closes embed). Setup wizard `setup:*` clicks pass through silently.
- `src/handlers/modal.rs` — `hc:<id>:confirm:<rid>`: records confirmed reaction in DB,
  edits headcount message via HTTP, acknowledges with ephemeral "✅ Confirmed!".
- `src/main.rs` — global `event_handler` in `FrameworkOptions` dispatches `Component`
  and `Modal` interactions to their handlers.
- **`cargo build` passes** (15 dead-code warnings for Phase 5+ scaffolding, no errors).

Phase 5 prerequisites are now met: the `/headcount` command works end-to-end.
The `hc:<id>:start` handler has a `// TODO Phase 5` marker where `start_run` will plug in.

### 2026-04-23 — Phase 5 complete

Landed:
- `src/db/run.rs` — DB layer: `create`, `set_message_id`, `get`, `list_active`,
  `set_status` (stamps `ended_at` when flipping to `ended`), `set_location`,
  `set_party`, `set_leader`, `set_voice_channel`; `add_participant`
  (NULL-safe upsert via `NOT EXISTS` on the COALESCEd unique index),
  `remove_participant_all`, `list_participants`, `list_user_ids`. Introduces
  a `Participant` struct (user_id, dungeon_reaction_id, confirmed).
- `src/db/mod.rs` — exposes `run` module.
- `src/embeds/run.rs` — `build()` active embed (per-reaction fields with
  `{count}/{required}`, ✅ for confirmed participants, "Joined" roster) +
  Join / Leave / Control Panel row + per-confirmation-reaction Confirm
  buttons; `build_ended()` grey embed preserving the per-item breakdown;
  `control_panel()` leader-only ephemeral with Set Location / Set Party /
  Transfer Leader / End Run.
- `src/services/raid.rs` — `start_run(serenity_ctx, pool, guild_id, tier,
  template, raid_channel_id, leader_user_id, headcount_id)`. Low-level shape
  so both `/run` and the `hc:<id>:start` handler can call it. Migrates
  headcount reactions into `run_participants`: `requires_confirmation`
  reactions carry their `confirmed` flag and keep their `dungeon_reaction_id`;
  plain interest reactions become NULL (joined, no declared item). Pings the
  template's `notification_role_id` on the run message when set.
- `src/commands/run.rs` — `/run <dungeon> [tier]` slash command with
  autocomplete for both args, `StartRun` permission check, tier auto-resolve
  for single-tier guilds, fallback to headcount channel if `raid_channel_id`
  is unset. Registered in `commands/mod.rs`.
- `src/handlers/run.rs` — new module: component + modal routing for
  `run:*` custom_ids:
  - `run:<id>:join` — add self as NULL-item participant; leader-leave
    blocked (must transfer or end first).
  - `run:<id>:leave` — remove all of the user's participant rows.
  - `run:<id>:cp` — open the ephemeral Control Panel (leader only).
  - `run:<id>:loc` / `run:<id>:party` — leader opens a Modal pre-filled
    with the current value; submission trims input, treats empty as clear,
    and refreshes the public embed via HTTP (modal submissions can't
    UpdateMessage another message).
  - `run:<id>:transfer` → `run:<id>:xfer` — leader opens a UserSelect
    ephemeral, submission updates leader, auto-joins the new leader,
    refreshes the public embed, dismisses the ephemeral.
  - `run:<id>:end` — leader flips status to `ended`, public message becomes
    the grey ended embed with no components; best-effort log follow-up in
    the guild's `log_channel_id` if set.
  - `run:<id>:confirm:<rid>` / `run:<id>:confirm_do:<rid>` /
    `run:<id>:confirm_cancel` — two-click ephemeral confirm flow identical
    in spirit to the headcount confirm (single-button yes/no, no modal).
- `src/handlers/component.rs` — top-level dispatcher now routes `run:*` to
  `handlers::run::handle_component` before falling through to the existing
  `hc:*` logic. `handle_start` no longer has its Phase 5 TODO — it closes
  the headcount embed, flips status to `converted`, then calls
  `services::raid::start_run` with the headcount id so participants
  auto-migrate.
- `src/handlers/modal.rs` — dispatches `run:*` modal submissions to
  `handlers::run::handle_modal`; no other modal flows in use (headcount
  confirm is one-click, not a modal).
- `src/handlers/mod.rs` — exposes new `run` submodule.
- **`cargo build` passes** (16 warnings, all for dead code in Phase 6+
  scaffolding, no errors).

Phase 6 prerequisites are now met. End-to-end flow works: `/setup` →
`/headcount dungeon` (or `/run dungeon`) → participants click Join/Confirm →
leader clicks Control Panel → Set Location / Party / Transfer / End. Runs
"continue until the leader ends it" (Phase 5 item 25) by default — chained
re-run prompts are a future polish item.

### 2026-04-24 — Rework R1 complete (scraper correctness + emoji purge)

Landed:
- `migrations/20260424000001_bag_tiers.sql` — new `bag_tiers` lookup
  table (8 rows, sort_order 0=brown → 7=white, each with a unicode
  fallback emoji); `bag_tier` column added to `bot_emoji` with an index.
- `src/cli/sync_wiki.rs`:
  - `slug_from_display` strips straight `'` and curly `\u{2019}` before
    the alphanumeric pass, so "Oryx's Sanctuary" now yields
    `oryxs_sanctuary` (not `oryx_s_sanctuary`). Four unit tests cover
    straight apostrophe, curly apostrophe, multi-word, and collapse of
    runs of punctuation.
  - `scrape_dungeon_list` now slices the raw HTML to just the
    "Regular Dungeons" section via a new `extract_regular_dungeons_section`
    helper (uses a flexible `find_heading_offset` text-matcher).
    Special Event, Other Dungeons, Mini Dungeons sections are ignored
    entirely. A hardcoded `EXCLUDED_SLUGS` denylist drops Court of Oryx,
    Oryx's Castle, Oryx's Chamber, Wine Cellar.
  - `extract_drops_section` rewritten to match any of `Drops of Interest`,
    `Notable Drops`, `Drops` (case-insensitive) so dungeons like Snake
    Pit that name the section differently land their drops.
  - New `scrape_item_page` fetches each drop's wiki page (~250ms
    throttle), finds the "Loot Bag" table row, and extracts both the
    bag-colour word (→ `bag_tier`) and the row's img src (→ bag icon
    URL). Results are cached per-run keyed on drop img URL so duplicates
    across dungeons aren't re-fetched.
  - First time each bag tier's icon is seen, it's uploaded as
    `bag_<tier>` under `bot_emoji` (category `ui`). Subsequent drops of
    the same tier skip the re-upload.
  - `DropItem` struct now carries `item_wiki_path` (optional) and the
    `is_white_bag` flag is gone — bag-tier classification is per-emoji
    in `bot_emoji.bag_tier`, not per-drop-per-dungeon.
  - `showcase_emoji` on `dungeon_templates` now receives every scraped
    drop, not just white-bag items. Bag-tier grouping in the R2
    renderer takes over the "which drops are interesting enough" job.
  - New `--purge` flag (prompted Y/N via stdin): deletes every
    application emoji the bot owns and TRUNCATEs `bot_emoji`, then runs
    the usual scraper. One-shot op to retire the legacy
    `oryx_s_*` / `pirate_s_*` names.
- `src/db/emoji.rs` — `upsert` gained a ninth arg `bag_tier:
  Option<&str>`, with `COALESCE(EXCLUDED.bag_tier, bot_emoji.bag_tier)`
  so later writes without a tier don't erase a previously-found one.
  New `truncate` helper used by `--purge`. SELECT column lists updated.
- `src/db/models.rs` — `BotEmoji.bag_tier: Option<String>` added.
- `src/templates/dungeons.rs` — `oryx_s_sanctuary` renamed to
  `oryxs_sanctuary` (`.name` field only; emoji logical name
  `portal_sanctuary` unaffected).
- `src/curation.rs` — `Curation::migrate_legacy_slugs()` rewrites stale
  apostrophe-expanded keys (`_s_` → `s_`, trailing `_s` → `s`) in
  memory; sync-wiki calls it at startup and `save()`s if anything
  changed, so `data/curation.json` self-heals.
- `src/main.rs` — `--purge` wired through the CLI.
- **`cargo build` passes** (16 dead-code warnings, all for R2+/future
  phases; no errors).
- **`cargo test`** passes: 4 new slug tests (`oryxs_sanctuary`,
  `pirates_cave`, `snake_pit`, `d_o_g_realm`).

R2 prerequisites are now met: every scraped drop has a `bag_tier`
(if classifiable), bag icons exist as `bag_<tier>` emojis, and the
`dungeon_templates.showcase_emoji` column holds every drop for each
dungeon. The R2 renderer can group by tier and filter by threshold.

Runbook:
1. `cargo run -- sync-wiki --purge` once (interactive Y/N) to wipe the
   legacy emoji set and rebuild under the fixed slug rules.
2. `cargo run -- sync-wiki` on subsequent runs.

### 2026-04-23 — Rework R2 complete (bag-tier rendering + threshold)

Landed:
- `migrations/20260424000002_guild_loot_tier_threshold.sql` — new
  `guild_loot_tier_threshold(guild_id, dungeon_template_id, tier_name)`
  table, composite PK, FK to `bag_tiers(name)`. Absence of a row =
  default `white` (strictest).
- `src/db/models.rs` — added `BagTier { name, sort_order, default_emoji }`.
- `src/db/loot.rs` (new) — `list_bag_tiers`, `resolve_bag_emoji`
  (prefers the `bag_<tier>` application emoji, falls back to
  `bag_tiers.default_emoji` unicode literal), `get_threshold`
  (defaults to `white` on miss), `set_threshold` (upsert).
- `src/db/mod.rs` — exposes the `loot` module.
- `src/embeds/mod.rs` — new `build_loot_fields` helper groups a
  dungeon's `showcase_emoji` by `bot_emoji.bag_tier`, emits one embed
  field per tier ≥ threshold in descending order (white first). Drops
  without a `bag_tier` classification are silently skipped. Added
  `render_bot_emoji` + `tier_display_name` helpers.
- `src/embeds/headcount.rs` — `build` signature loses `tier_name`,
  gains `bag_tiers: &[BagTier]` and `threshold: &str`. Footer
  (`Tier: {name}`) removed. Loot fields appended after reaction
  fields. `build_closed` loses `tier_name` and its footer too.
- `src/embeds/run.rs` — same treatment: `build` and `build_ended`
  swap `tier_name` for `bag_tiers` + `threshold`, drop the footer,
  and append loot fields after the "Joined" roster.
- `src/services/raid.rs` — `start_headcount` and `start_run` load
  bag tiers + threshold per call and thread them through to the
  embed builders.
- `src/handlers/component.rs` — four call sites (`rebuild_and_update`,
  `handle_confirm_click`, `handle_start`, `handle_cancel`) updated.
  The two rebuild paths drop their `tier` DB load (no longer needed
  for the footer); `handle_start` keeps it for `raid_channel_id`.
- `src/handlers/run.rs` — `rebuild_and_edit_message` and `handle_end`
  drop their `tier` loads and pass bag tiers + threshold instead.
- `src/commands/config.rs` (new) — `/config threshold <dungeon> <tier>`
  subcommand, ConfigureGuild-gated, with autocompletion over both
  guild-visible dungeons and bag tier names. Persists via
  `db::loot::set_threshold`. `/config` is subcommand-required so
  future settings (`log_channel`, `notification_channel`, …) can
  slot in as sibling subcommands.
- `src/commands/mod.rs` — registers `/config`.
- **`cargo build` passes** (16 dead-code warnings for future phases,
  no errors).
- **Migration applied** against the local dev DB
  (`sqlx migrate info` shows 4/4 installed).

R3 prerequisites are now met: the renderer filters drops by bag tier
per dungeon, guild admins can raise or lower the threshold with
`/config threshold`, and the tier footer is gone from every embed
variant.

Note: the scraper step from the R2 description ("stops filtering
`showcase_emoji` to white-only; writes every drop") already landed in
R1 — confirmed in that progress entry.

### 2026-04-24 — R1 hotfix: bag detection + shiny tier

Two issues surfaced after R2 landed and `sync-wiki` was re-run with an
empty `bot_emoji.bag_tier` histogram: no drops were classifying, and
shiny variants were missing entirely.

- **Bag detection was broken.** `scrape_item_page` read `row.text()`
  to classify the "Loot Bag" row, but RealmEye stores the colour in
  the image's `alt`/`title` attributes
  (`<img alt="Assigned to White Bag">`). Fixed by iterating the
  images inside the row and matching colour words against the
  combined alt+title string.
- **New `shiny` bag tier** (migration
  `20260424000003_shiny_bag_tier.sql`): `('shiny', 8, '✨')`, above
  `white`. No bag-icon exists on RealmEye for shinies; the renderer
  falls back to the unicode ✨ unless someone uploads a custom
  `bag_shiny` application emoji later.
- **Scraper now extracts shiny sprites.** `scrape_item_page` does a
  second pass for `<img alt="... (Shiny)">` anywhere on the item
  page (excluding the projectile variant which uses "Shiny … Projectile").
  When found, `<logical>_shiny` is uploaded as a distinct drop emoji
  with `category='drop_shiny'`, `bag_tier='shiny'`, and appended to
  the dungeon's `showcase_emoji`. Shinies inherit their parent's
  curation decision.
- `BAG_TIERS_ORDERED` extended with `shiny` at the end for symmetry
  with the lookup table (unused for Loot-Bag-row classification).
- No schema change to `bot_emoji`; existing rows backfill on the
  next upsert via `COALESCE(EXCLUDED.bag_tier, bot_emoji.bag_tier)`.
- **`cargo build` passes**; migration applied; operator ran
  `cargo run -- sync-wiki` successfully (wiki-snapshot updated).

### 2026-04-24 — Rework R3 complete (setup simplification + `/pingroles`)

Landed:
- `migrations/20260424000004_tier_runs_channel.sql` — adds
  `tiers.runs_channel_id BIGINT`, backfilled via
  `COALESCE(raid_channel_id, headcount_channel_id)`. Legacy
  `raid_channel_id` / `headcount_channel_id` / `guilds.notification_channel_id`
  survive this phase (R3 dual-writes; R4 drops them).
- `src/db/models.rs` — `Tier` gains `runs_channel_id: Option<i64>` and a
  `runs_channel()` helper that returns the unified value with fallback to
  the legacy columns.
- `src/db/tier.rs` — every SELECT/RETURNING updated; `update()` replaced its
  `raid_channel_id + headcount_channel_id` pair with a single
  `runs_channel_id` parameter that dual-writes to the legacy columns for the
  R3→R4 window.
- `src/db/dungeon.rs` — new `set_notification_role(guild_id, template_id,
  role_id)`: updates in place when the template is already guild-specific,
  otherwise clones the global template into a guild-specific row (clones are
  preferred by `list_for_guild`'s `DISTINCT ON (name)` + `NULLS LAST`).
- `src/services/raid.rs` — `start_headcount` now pings
  `dungeon_templates.notification_role_id` just like `start_run` already
  did, so subscribers hear about a headcount the moment it opens.
- `src/commands/headcount.rs` + `src/commands/run.rs` +
  `src/handlers/component.rs::handle_start` — all three post targets now
  prefer `Tier::runs_channel()`, falling back to whichever channel the
  headcount originally posted in if the unified column is still null.
- `src/commands/setup.rs`:
  - Intro + dashboard + summary copy rewritten to describe a single runs
    channel ("headcounts and runs both live there").
  - Quick setup creates one `{slug}-runs` text channel under a "Raids"
    category, plus `🚀starship-log` (falls back to plain `starship-log` if
    Discord rejects the emoji prefix). Notifications-channel concept is
    gone end-to-end: no channel created, no dashboard section, no
    `setup:section:notif` handler. `set_notification_channel` DB function
    survives unused (R4 drops the column).
  - First-tier section (`tier_view` + `section_first_tier`) collapsed to a
    single "Runs channel" select; `TierDraft` holds `runs_channel` +
    `access_roles` only.
  - `create_default_channels` returns one `ChannelId`; also accepts the
    legacy `{slug}-raid-room` name when picking up an existing channel,
    so mid-migration guilds don't double-create.
- `src/commands/tier.rs` — `/tier edit` exposes a single `runs_channel`
  param; `/tier list` renders one "Runs: <#…>" line per tier.
- `src/commands/pingroles.rs` (new) — one root command with three admin
  subcommands and a default self-service flow:
  - `/pingroles` (anyone) — paginated ephemeral picker (10 dungeons/page)
    with a StringSelect per page, `default_selection` preloaded from the
    user's current role membership. Prev/Next buttons rotate pages;
    selections from each page accumulate into an in-memory `HashSet<i32>`.
    On **Apply** the command re-fetches the Discord member, computes
    desired vs. current role diff scoped to Starship-managed roles only,
    and fires `member.add_role` / `member.remove_role` with a 3× retry and
    200→400→800 ms backoff per mutation. Partial failures produce a clear
    per-role error list without aborting the batch.
  - `/pingroles set <dungeon> <role>` (ConfigureGuild) — binds an existing
    role.
  - `/pingroles unset <dungeon>` (ConfigureGuild) — clears the binding.
  - `/pingroles create <dungeon>` (ConfigureGuild) — creates (or reuses) a
    mentionable role named `"<display name> Pings"` and binds it.
- `src/commands/notifications.rs` — deleted.
- `src/commands/mod.rs` — unregisters `notifications`, registers
  `pingroles`.
- **`cargo build` passes** (17 dead-code warnings for future phases, no
  errors). **`cargo test`** passes (5/5 slug + section tests).
- **Migration applied** against the local dev DB (`sqlx migrate info`
  shows 6/6 installed).

R4 prerequisites are now met: every code path writes + reads
`runs_channel_id`; the legacy `tiers.raid_channel_id` /
`tiers.headcount_channel_id` / `guilds.notification_channel_id` columns
remain dual-written for safety but have no live readers outside the SELECT
column lists in `db::tier`. R4 can drop them, strip the `Tier` struct
fields + the `runs_channel()` fallback chain, and delete the
`set_notification_channel` helper in one pass.

### 2026-04-24 — Rework R4 complete (reactions + cleanup)

Landed:
- `migrations/20260424000005_r4_reactions.sql` — drops
  `headcount_reactions` + `run_participants` tables; drops the legacy
  `tiers.raid_channel_id`, `tiers.headcount_channel_id`, and
  `guilds.notification_channel_id` columns. No data migration — R4
  explicitly retires per-user lifecycle tracking.
- `src/main.rs` — `r4_migration_preflight` runs before
  `sqlx::migrate!` and refuses to apply the migration if active
  headcounts or runs exist on the pre-R4 schema. Bypass via
  `STARSHIP_ALLOW_MIGRATION=1`. Auto no-op once the migration is
  applied (uses `to_regclass('public.headcount_reactions')`).
- `src/services/reactions.rs` (new) — `attach_reactions` fires one
  `create_reaction` per required emoji with 5× retry + exponential
  backoff on 429/5xx. `ping_organizer_on_failure` @-mentions the
  organizer in the channel when any reaction didn't stick.
- `src/services/raid.rs` — `start_headcount` + `start_run` both call
  `attach_signup_reactions` after the message posts. Participant
  migration from headcount → run is gone.
- `src/services/permission.rs` — new `can_organize` (leader OR
  ManageRuns OR superadmin OR Discord admin) for component handlers,
  plus `can_organize_from_interaction` sugar that reads the caller off
  a `ComponentInteraction`.
- `src/handlers/component.rs` — cut `hc:*:react:*`, `hc:*:confirm:*`,
  `hc:*:confirm_cancel`. `hc:*:start` and `hc:*:cancel` are now
  organizer-gated.
- `src/handlers/run.rs` — cut `run:*:join`, `run:*:leave`,
  `run:*:confirm*`. Kept `cp` / `loc` / `party` / `transfer` / `xfer` /
  `end`, all organizer-gated. Modal submissions reconstruct the gate
  from `modal.member` via `can_organize`.
- `src/embeds/headcount.rs` — `build()` drops the `counts` parameter
  and per-reaction fields. Only `Start Run` + `Cancel` buttons remain.
  A `**React with:** …` inline list tells users what to click.
- `src/embeds/run.rs` — `build()` drops per-reaction fields and the
  `👥 Joined` roster. `build_ended()` is minimal: title, leader, and
  optional location. Public components collapsed to the Control Panel
  button.
- `src/templates/dungeons.rs` — every builtin has
  `requires_confirmation: false`. O3 gains `wine_cellar_incantation`
  and keeps all three rune reactions (5 reacts with ✅). Void swaps to
  `lost_halls_key` + `vial_of_the_void`. Cultist drops to just
  `lost_halls_key`.
- `src/db/dungeon.rs::seed_templates` — reaction upserts now DO UPDATE
  (display_name / emoji / sort_order / requires_confirmation) so
  template changes propagate on bot restart.
- `src/db/models.rs` — `Guild.notification_channel_id` and the legacy
  `Tier` channel columns + `Tier::runs_channel()` helper are gone.
  `HeadcountReaction` struct removed.
- `src/db/run.rs` — participant CRUD removed (`Participant`,
  `add_participant`, `remove_participant_all`, `list_participants`,
  `list_user_ids`).
- `src/db/headcount.rs` — reaction CRUD removed
  (`reaction_counts`, `get_user_reaction`, `add_reaction`,
  `remove_reaction`). `HeadcountReaction` deleted.
- `src/db/guild.rs` — `set_notification_channel` + column refs gone.
- `src/db/tier.rs` — `update()` writes a single `runs_channel_id`;
  SELECT column lists no longer include the dropped legacy columns.
- `src/cli/upload_emoji.rs` (new) + `starship upload-emoji` CLI —
  one-off manual emoji upload via `ApplicationEmojiClient`. Takes
  `--name`, `--file`, optional `--discord-name`, `--category`,
  `--bag-tier`. Idempotent against Discord's existing emoji set.
- **`cargo build` passes** (14 dead-code warnings, no errors).
  **`cargo test`** passes (5/5).
- **Migration applied** against the local dev DB
  (`sqlx migrate info` shows 7/7 installed).

Operator follow-up:
- The Void's `vial_of_pure_darkness` and Cultist's `cultist_key`
  reactions linger in the DB as orphans (seed_templates only upserts
  the new set). Cleanest path: `starship upload-emoji --name
  wine_cellar_incantation --file ...` if the scraper didn't pick it
  up, then either leave the orphans (harmless — they just render as
  an extra unused reaction on old-data templates) or prune via
  `/dungeon edit` once the UI supports per-reaction CRUD.
- If `wine_cellar_incantation` / `vial_of_the_void` aren't in
  `bot_emoji` yet, the reaction attachment will silently skip them
  (no emoji resolver); upload them via the new CLI before running a
  raid.

### 2026-04-24 — Phase 6 complete (temp VC lifecycle)

Narrowed from the original Phase 6 scope: items 26 (create) and 28
(cleanup) only. Item 27 (join enforcement) is deferred — users trust
each other for now. No songbird; plain REST channel CRUD.

- `Cargo.toml` — `songbird` dep removed; `"voice"` feature removed
  from the serenity feature list. Nothing in `src/` imported either.
- `src/services/voice.rs` — `create_temp_vc(ctx, guild_id,
  runs_channel_id, name)` looks up the runs channel's parent category
  via `to_channel` and creates a voice channel of the given name
  under it (guild root if no parent). `delete_temp_vc(http,
  channel_id)` is fire-and-forget with warn-logging — once a run is
  ended we don't care why a delete failed.
- `src/services/raid.rs::start_run` — when
  `template.requires_vc`, creates `"{display_name} #{run_id}"` in
  the runs channel's category, persists via
  `db::run::set_voice_channel`, and updates the in-memory `Run` so
  the embed renders the VC line (`**Voice:** <#…>`) on first post.
  Failure is logged and the raid posts without a VC rather than
  aborting.
- `src/handlers/run.rs::handle_end` — after flipping status to
  `ended`, deletes the VC if the run had one. Runs before the
  message edit so the cleanup happens even if the edit errors.
- `src/embeds/run.rs` — already rendered `voice_channel_id` from the
  earlier scaffold; no change needed.
- **`cargo build` passes** (13 dead-code warnings, no errors — one
  down from R4, matching the songbird removal).
  **`cargo test`** passes (5/5).

VC-raid end-to-end flow: operator sets `requires_vc = true` on a
template → `/run` or `/headcount` → `Start Run` → bot creates
`{dungeon} #{id}` VC in the runs-channel category, mentions it in
the embed → leader ends run → bot deletes VC.

Deferred until later:
- Join enforcement / auto-move / kick-on-missing (original item 27).
- VC orphan recovery on bot restart (Phase 7 crash recovery).
- Per-tier VC category override (currently always the runs
  channel's parent).

### 2026-04-24 — Phase 6.5 complete (dungeon config refactor)

Goals: linearise the dungeon-template data flow (kill the two-source
collision between hardcoded builtins and the scraper), move loot-tier
threshold from per-dungeon to per-guild, and delete the curation
module that overrides.json now subsumes.

Landed:
- **Migration `20260424000006_per_guild_threshold.sql`** — adds
  `guilds.loot_tier_threshold TEXT NOT NULL DEFAULT 'white'
  REFERENCES bag_tiers(name)`, drops `guild_loot_tier_threshold`.
- `src/db/loot.rs` — `get_threshold(pool, guild_id)` /
  `set_threshold(pool, guild_id, tier)`. Dungeon parameter gone.
- `src/commands/config.rs` — `/config threshold <tier>` (no dungeon
  arg). Autocomplete list is just the 8 bag tiers.
- `src/services/raid.rs` (two call sites) +
  `src/handlers/run.rs::rebuild_and_edit_message` — all load the
  guild-level threshold now.
- `src/db/models.rs::Guild` + `src/db/guild.rs` SELECT/RETURNING
  lists — gain `loot_tier_threshold: String`.
- **`src/templates/mod.rs`** (new) — authors the dump+overrides
  pipeline:
  - `WikiDump` / `WikiDungeon` / `WikiEmoji` (serde types for
    `data/wiki_dump.json`; replaces `WikiSnapshot` et al.).
  - `Overrides` (BTreeMap<String, DungeonOverride>) backed by
    `data/dungeon_overrides.json`. Two patterns: `extends: "<wiki>"`
    (additive clone) or no-extends (patch). Scalar fields are
    Option<T>; `reactions` is Option<Vec<OverrideReaction>>.
  - `load_and_seed(pool)` — validates extends targets, merges dump
    + overrides into effective templates, upserts each to Postgres.
    Default reactions (when override doesn't set them) =
    interest(✅) + key (if dump has one), both
    `requires_confirmation: false` (fixes the R4 gap where sync-wiki
    had key=true).
  - Three unit tests: `extends_clones_source_and_applies_overrides`,
    `extends_unknown_target_errors`, `patch_override_replaces_scalars_and_reactions`.
- `src/db/dungeon.rs` — `seed_builtins` / `seed_templates` deleted.
  `upsert_global_template` gains `message_title` +
  `message_description` params and is now authoritative on conflict
  (no more COALESCE-preserve; the seeder writes what the files say).
  New `delete_reactions_not_in(pool, template_id, keep_names)` for
  orphan-reaction cleanup after the desired set is upserted —
  closes the long-standing R4 orphan issue.
- **`data/dungeon_overrides.json`** (new, committed) — the 5
  non-trivial migrations from the deleted hardcoded builtins, plus
  `fullskip_void` as the motivating `extends` example:
  - `oryxs_sanctuary` (5 reactions + requires_vc),
  - `the_void` (3 reactions, requires_vc:false explicit),
  - `fullskip_void` (`extends: "the_void"` + requires_vc:true +
    display_name + message_title),
  - `the_shatters` (3 reactions + requires_vc),
  - `lost_halls` (requires_vc),
  - `cultist_hideout` (requires_vc).
- **`data/wiki_dump.json`** — renamed from `data/wiki-snapshot.json`
  (git mv). Committed so fresh clones seed without first running
  `sync-wiki`.
- `src/cli/sync_wiki.rs` — rewritten to write only
  `data/wiki_dump.json` + `bot_emoji` rows. The direct
  `upsert_global_template` / `upsert_reaction` calls are gone; so
  are the curation gatekeepers (every scraped drop goes in the
  dump). Shinies are now appended to the dump's `drops` so the
  seeder's showcase derivation picks them up.
- `src/main.rs` — `mod curation` removed; `Curate` CLI subcommand
  removed; `run_bot` now calls `templates::load_and_seed(&pool)`
  in place of `seed_builtins`.
- **Deleted:** `src/templates/dungeons.rs` (the 9 BUILTIN_TEMPLATES),
  `src/curation.rs` (Curation + migrate_legacy_slugs + WikiSnapshot),
  `src/cli/curate.rs` (the curate subcommand).

Verification:
- `cargo build` — 13 warnings (unchanged baseline). No errors.
- `cargo test` — 8 passed (5 pre-existing slug/section + 3 new
  merge tests).
- `sqlx migrate info` — 8/8 installed; the new migration applied
  cleanly.
- Live seed on the dev DB (via `cargo run -- bot`) — "seeded
  dungeon templates dungeons=62 overrides=6". Verified in psql:
  `oryxs_sanctuary` has interest + wine_cellar_incantation + 3
  runes (sort_order 0-4); `fullskip_void` exists with
  requires_vc=true and 3 inherited reactions (interest +
  lost_halls_key + vial_of_pure_darkness); the 5 VC dungeons all
  flipped to `requires_vc=true`, the 3 non-VC dungeons stayed
  false, and `the_void` correctly flipped to false.

Operator follow-ups / notes:
- Existing DB carries ~26 global template rows from older
  sync-wiki runs that aren't in the current `wiki_dump.json`. The
  new seeder doesn't delete them (to avoid nuking guild overrides
  that might share a name by accident). They're stale but
  harmless — they just render in `/headcount` autocomplete until
  pruned by hand.
- If the threshold ever needs to diverge per-guild beyond "the
  floor," we'd add a new table rather than resurrect the per-
  dungeon knob — simpler semantics have real value.

### 2026-04-24 — Phase 7a complete (logging + error handling sweep)

Targeted at Docker (`docker logs`) and systemd (`journalctl -u starship`)
output side-by-side.

Landed:
- `Cargo.toml` — `tracing-subscriber` gains the `json` feature.
- `src/main.rs::init_tracing` — split out from `main()`. Default filter
  `starship=info,serenity=warn,sqlx=warn,info` keeps third-party noise
  quiet without an explicit `RUST_LOG`. Set `RUST_LOG_FORMAT=json` to
  switch to one JSON object per line for log shippers; otherwise the
  human-readable pretty format. Both go to stdout.
- `.env.example` — documents `RUST_LOG_FORMAT=` alongside the existing
  `RUST_LOG=`. `RUST_LOG=` left blank so the in-code default applies.
- `src/main.rs::on_error` — replaced the bare `poise::builtins::on_error`
  delegate with a structured-logging match. Captures command name,
  user_id, guild_id, full error chain on `Command` failures; warns with
  caller info on `CommandCheckFailed`; logs event name + error chain on
  `EventHandler` errors; categorises every other variant via a
  `framework_error_kind` helper that returns a static label (avoids
  requiring `Debug` on `BotData`).
- Lifecycle spans via `#[tracing::instrument]`:
  - `services::raid::start_headcount` → fields `guild_id`, `leader_id`,
    `tier`, `dungeon`, `channel_id`, `hc_id` (recorded post-create).
    Emits `info!("headcount created")` after the DB insert.
  - `services::raid::start_run` → fields `guild_id`, `leader_id`, `tier`,
    `dungeon`, `requires_vc`, `run_id` (recorded post-create). Emits
    `info!("run created")` after the DB insert.
  - `handlers::component::handle` → fields `custom_id`, `user_id`,
    `guild_id`. Every nested log (reactions retries, voice failures,
    template lookups) inherits this context.
  - `handlers::modal::handle` → same shape as components.
- Structured fields:
  - `error = ?e` (Debug, prints the full anyhow chain) replaces
    `error = %e` (Display, swallows `.context()` chains) at every
    warn/error site in `services/`, `commands/setup.rs`, and
    `templates/mod.rs`.
  - `templates::load_and_seed` warn for missing files now carries
    `dump_path` and `overrides_path` as fields.
  - `templates::merge` orphan-override warn carries `override_name`.
  - `commands/setup.rs::quick_setup` failure carries `guild_id`.
- Warn-and-continue audit on terminal flows:
  - `handlers::run::handle_end` — every step after `db::run::delete`
    (template load, emoji map load, message edit, interaction reply,
    audit log post) now warns and continues instead of bubbling. The
    DB delete is the commit point; nothing after it should turn into a
    user-visible "interaction failed" toast for an already-ended run.
  - `handlers::headcount::handle_confirm_start` — the post-conversion
    "strip buttons" message edit was a silent `let _ = …` swallow;
    promoted to a structured `tracing::warn!` so the failure is at
    least visible in logs.
- **`cargo build` passes** (12 dead-code warnings — one less than before
  because `init_tracing` consolidates the old inline subscriber).
  **`cargo test`** passes (11/11).

Phase 7b (containerised deploy) prerequisites are now met: stdout is
the only sink, the format is selectable via env, and every lifecycle
event carries enough structured context that JSON logs in production
will be greppable by guild_id / run_id / hc_id without hunting for
formatted strings.

### 2026-04-24 — Production-grade audit Phase A complete (lint + format baseline)

First chunk of the production-grade audit triggered by the CLAUDE.md
update (new Rust quality rules). Goal of Phase A: get to a zero-warning
baseline and pin the toolchain so CI in Phase E has something
unambiguous to enforce.

Landed:
- `cargo fmt` sweep across 23 files (88 mechanical diffs, isolated to
  its own commit `Phase A.1: cargo fmt sweep`).
- All 12 `cargo build` warnings resolved:
  - Real fix: `templates::default_reactions` signature changed from
    `Option<&WikiEmoji>` to `bool`. The function never read the wiki
    emoji bytes — keys render as the native `🔑`. The unused-binding
    warning was the symptom of an over-broad parameter.
  - Deleted as dead: `db::emoji::get_by_logical_name` (handlers use
    `get_all_as_map`), `services::permission::require_str` (component
    handlers use the more specialized `can_organize_from_interaction`).
  - `#![allow(dead_code)]` at `db/models.rs` module level with an
    anchoring comment: every field is required by `sqlx::FromRow` to
    populate from `SELECT *`-style queries even when no caller reads
    it. Trimming would force the SQL to drop the column and re-add
    it on the first new caller.
  - `#[allow(dead_code)]` on `services::permission::Action` enum with
    a comment: variants are the authoritative permission registry that
    `ALL_ACTIONS` and `db::permission::check` match against, even when
    no command currently calls `require(Action::X, …)` directly.
- All 10 unique `cargo clippy` warnings resolved (full list in commit
  `Phase A.2`). Notable:
  - Test module in `cli/sync_wiki.rs` moved to file bottom so
    `discord_name`, `absolute_url`, `purge_all` come before it.
  - Four `too_many_arguments` sites annotated with
    `#[allow(clippy::too_many_arguments)]` and a comment deferring the
    parameter-struct refactor to **Phase D**, where it sits naturally
    next to the snowflake-newtype work. Refactoring now would churn
    every caller for purely cosmetic reasons.
- New tooling files:
  - `rustfmt.toml` — `edition = "2021"`, `max_width = 100`.
  - `rust-toolchain.toml` — pin to `1.95.0` with `rustfmt` + `clippy`
    components, `profile = "minimal"`. First contributor checkout
    (and Phase E's CI image) installs the same toolchain
    automatically.
- All four gates pass on the branch:
  - `cargo fmt --check` — 0 diffs
  - `cargo build` — 0 warnings
  - `cargo clippy --all-targets -- -D warnings` — 0 warnings
  - `cargo test` — 11 passed, 0 failed (no test changes; the cli test
    module just moved file position).

**Deferred to later phases (intentional, called out so they aren't
forgotten):**
- 49 `ctx.guild_id().unwrap().get() as i64` instances → **Phase B**
  (single `guild_id_i64(ctx)` helper).
- 9 `Selector::parse(...).unwrap()` in `cli/sync_wiki.rs` → **Phase B**
  (`once_cell::sync::Lazy`).
- DB-arg parameter structs for the four `#[allow(too_many_arguments)]`
  sites → **Phase D**.
- Doc coverage gap (~30% of public items documented) → **Phase E**.

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
