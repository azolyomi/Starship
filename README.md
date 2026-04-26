# Starship

A Discord bot that runs *Realm of the Mad God* raids end-to-end: posts a
rich headcount embed, tracks signups and required-item reactions,
converts a confirmed headcount into a live run with a leader-only
control panel, manages temp voice channels, and cleans up after itself
when the run ends or idles out.

Designed for guilds running organised dungeons (Oryx 3, The Void,
Cultist Hideout, Lost Halls, Snake Pit, etc.) where the social
overhead of "who's coming, who has the key, what location are we
on" eats more time than the dungeon itself.

## Features

- **Headcount → run lifecycle** with reaction-driven signups and
  per-dungeon required-item gating (key, runes, vials, …).
- **Per-tier raid channels and notification roles**, opt-in via
  `/pingroles`. Members pick which dungeons they want to be pinged for.
- **RealmEye verification** — `/verify` issues a one-time code, the
  user pastes it into their RealmEye description, and the bot scrapes
  to confirm before assigning the verified role and setting the
  nickname.
- **Self-organize** mode for tiers that don't need a single leader —
  members start their own runs from a sticky message; runs are
  reaped automatically when idle.
- **RealmEye wiki scraper** (`starship sync-wiki`) populates dungeon
  templates and uploads loot/bag-tier emojis as Discord Application
  Emojis owned by the bot, no per-guild emoji slots needed.
- **Crash-recovery orphan sweep** on every startup reconciles DB
  state with what's actually in Discord, so a hard kill never leaves
  rotting rows or zombie temp VCs.
- **24-hour idle auto-end** for runs whose leader forgot to close
  them out.

## Architecture

Single Rust binary, async tokio runtime, talking to one Postgres.
Discord interactions are stateless: every button's `custom_id`
encodes the entity ID it acts on, so a restart mid-flow doesn't
strand any UI — the next click is routed correctly without in-memory
state.

```
            ┌────────────────────────────┐
            │  Discord Gateway (WSS)     │
            └──────────────┬─────────────┘
                           │ events
                           ▼
   ┌──────────────────────────────────────────────┐
   │  starship  (single binary, tokio runtime)    │
   │  ┌────────────┐  ┌────────────┐  ┌─────────┐ │
   │  │ commands/  │  │ handlers/  │  │services/│ │
   │  │ (poise)    │  │ (custom_id │  │ (raid,  │ │
   │  │            │  │  router)   │  │ verify, │ │
   │  │            │  │            │  │  voice) │ │
   │  └─────┬──────┘  └─────┬──────┘  └────┬────┘ │
   │        └────────┬──────┴──────────────┘      │
   │                 ▼                            │
   │           ┌─────────┐                        │
   │           │  db/    │  (sqlx, compile-time   │
   │           │         │   checked queries)     │
   │           └────┬────┘                        │
   └────────────────┼────────────────────────────-┘
                    ▼
            ┌────────────────┐
            │  Postgres 16   │
            └────────────────┘
```

### Tech stack

| Layer | Choice | Why |
|-------|--------|-----|
| Language | Rust 2021 (edition), `tokio` | Memory-safe, low-latency, mature async |
| Discord | `serenity` 0.12 + `poise` 0.6 | Stable slash commands, components, modals |
| Voice | `songbird` 0.4 | Serenity-native VC management |
| Database | Postgres 16, `sqlx` 0.8 | Compile-time-checked queries |
| Migrations | `sqlx-cli` | Built-in tool, runs automatically on startup |
| Process | Docker compose | `restart: unless-stopped` is the watchdog |

### Project layout

```
src/
  main.rs            entry point, framework setup, error reporter
  config.rs          typed env loading via dotenvy, masked Debug
  cli/               non-bot subcommands (sync-wiki, upload-emoji)
  commands/          poise slash commands
  handlers/          custom_id router for buttons, modals, reactions
  services/          domain logic (raid, verification, voice, …)
  db/                schema-aware query modules (one per table family)
  embeds/            embed builders for headcount + run + listings
  templates/         seed loader for built-in dungeon templates
migrations/          sqlx migrations
data/                seed templates + curation overrides
deploy/              systemd unit (legacy bare-metal path)
.sqlx/               sqlx offline query cache (committed; see Development)
.githooks/           per-clone git hooks (activate via core.hooksPath)
```

## Development

Targets Linux/WSL2. The dev environment is identical to production,
just bare-metal Postgres instead of containerised.

### Prerequisites

- Linux or WSL2
- `curl`, `git`, `build-essential` (the setup script will install
  Rust, Postgres, and `sqlx-cli` if missing)

### Bootstrap

```bash
git clone https://github.com/azolyomi/Starship.git
cd Starship

./setup.sh                     # rustup + postgres + sqlx-cli + DB + migrations
git config core.hooksPath .githooks   # enable the .sqlx auto-update hook
```

Open `.env` and fill in:

- `DISCORD_TOKEN` — from https://discord.com/developers/applications
  → Bot tab → Reset Token. Enable **Message Content Intent** and
  **Server Members Intent** while there.
- `DISCORD_APPLICATION_ID` — same app, General Information.
- `DISCORD_TEST_GUILD_ID` *(optional, recommended)* — the guild ID
  to register slash commands instantly to during dev. Without this
  commands are registered globally and take ~1 hour to propagate.
- `REALMEYE_USER_AGENT` — quoted string with a contact, e.g.
  `"Starship/1.0 (contact: you@example.com)"`. Quotes matter —
  `deploy.sh` and the pre-commit hook source `.env`.

The full key reference is in `.env.example`.

### Run

```bash
cargo run --release -- bot
```

One-time per bot application, populate dungeon templates and emoji:

```bash
cargo run --release -- sync-wiki
```

### Code quality

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

### sqlx offline cache

Queries use `sqlx::query!` macros which validate against a live
Postgres at compile time. So the build can run inside Docker (no DB
reachable) and in CI, the resolved query metadata is cached in
`.sqlx/` and committed alongside the source.

The `.githooks/pre-commit` hook regenerates `.sqlx/` automatically
whenever a staged change touches a `.rs` file. If you've cloned
fresh, run `git config core.hooksPath .githooks` once to activate
it. The hook is a no-op when `DATABASE_URL` isn't loadable, so
commits from machines without the dev DB still go through.

## Deployment

Production runs as a Docker compose stack on a single VPS — Postgres
and the bot in two containers, persistent volume for the DB,
outbound-only networking. Compose's `restart: unless-stopped` is the
watchdog; no separate process supervisor needed.

The full launch checklist (Hetzner provisioning, hardening,
backups, observability, CI/CD) lives in [LAUNCH.md](LAUNCH.md).

Quick reference once a VPS is up:

```bash
ssh starship@<vps>
cd ~/Starship
git pull && ./deploy.sh           # rebuild + restart
docker compose logs -f bot        # tail logs
```

`deploy.sh` is intentionally tiny: it sources `.env` to fail fast on
missing secrets, runs `git pull`, then `docker compose up -d --build`.

## Configuration

All runtime config is environment-driven — no config files. `.env`
is gitignored and mode 600; `.env.example` is the committed
template. The `Config` struct in `src/config.rs` is the single load
point; tokens are masked (`MTE0…(72)`) in the `Debug` impl so they
can't accidentally end up in logs.

## Contributing

Personal project at the moment, but issues and PRs are welcome.
Before opening a PR:

- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` all green.
- If you touched any `sqlx::query!` macro, the pre-commit hook
  should have updated `.sqlx/` for you — verify it's in your
  commit.
- Project rules for AI agents are in [CLAUDE.md](CLAUDE.md);
  human contributors are welcome to follow the same style.

## License

MIT — see [LICENSE](LICENSE).
