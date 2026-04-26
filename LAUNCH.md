# LAUNCH.md — Production launch checklist

Goal: Starship running on a Hetzner VPS, talking to Discord, with backups
and observability. Implementation is done; this doc covers the missing
operational pieces and the order to land them in.

## Order of operations

Aim is to ship in one sitting. Each step is independently verifiable so
we can stop at any breakpoint and resume.

1. **Provision Hetzner VPS** (manual, ~20 min — see walkthrough below)
2. **First deploy + smoke test** (manual, ~30 min on the VPS)
3. **Postgres backup cron** (one shell script + crontab line)
4. **Tracing → Discord DM observability** (Rust code; one new module)
5. **GitHub Actions CI/CD** (one workflow file + one VPS deploy user)

Steps 3–5 can land in either order after the bot is up; backups first
because data loss is the worst failure mode.

---

## Step 1 — Provision Hetzner VPS

### 1.1 Account
- Sign up at https://www.hetzner.com/cloud (separate from "Hetzner Online"
  /Robot — Cloud is what we want).
- Payment: credit card or PayPal. First-time accounts sometimes get held
  for ID verification (passport scan, ~1 hour). Do this *before* you need
  the server.

### 1.2 SSH key (locally, before creating the server)
```bash
ssh-keygen -t ed25519 -C "starship-vps" -f ~/.ssh/starship_vps
# leave passphrase blank only if you're going to keep it on a single
# trusted laptop; otherwise set one and use ssh-agent.
```
Add `~/.ssh/starship_vps.pub` in Hetzner Cloud Console → Security → SSH
Keys before creating the server, so root login is key-only from the
first boot.

### 1.3 Create the server
- **Project**: create one called `starship`.
- **Image**: Debian 12.
- **Type**: CX22 (€4.51/mo, 2 vCPU shared / 4 GB / 40 GB NVMe). Pick
  CPX11 (AMD) only if a benchmark says you care.
- **Location**: Falkenstein/Nuremberg (DE) for EU users; Ashburn (US-East)
  for NA. Pick whichever is closest to most of the Discord guild.
- **SSH keys**: select the `starship-vps` key you uploaded.
- **Cloud Firewall** (sidebar): create one allowing inbound 22/tcp from
  *your* current IP only, plus 80+443 if you ever add a status page.
  Postgres is not exposed — the bot reaches it over the compose network.
- **Backups**: skip Hetzner's image-level backups (€0.91/mo). We do
  application-level pg_dump, which is cheaper and restorable to any
  host.

### 1.4 Initial hardening (one-time, on the VPS)
```bash
ssh -i ~/.ssh/starship_vps root@<vps-ip>

# system update
apt update && apt upgrade -y

# non-root user for the bot
adduser --disabled-password --gecos "" starship
mkdir -p /home/starship/.ssh
cp ~/.ssh/authorized_keys /home/starship/.ssh/
chown -R starship:starship /home/starship/.ssh
chmod 700 /home/starship/.ssh
chmod 600 /home/starship/.ssh/authorized_keys

# lock down ssh — disable root + password auth
sed -i 's/^#\?PermitRootLogin.*/PermitRootLogin no/' /etc/ssh/sshd_config
sed -i 's/^#\?PasswordAuthentication.*/PasswordAuthentication no/' /etc/ssh/sshd_config
systemctl restart ssh

# firewall (belt + braces with the Hetzner Cloud Firewall above)
apt install -y ufw fail2ban
ufw allow OpenSSH
ufw --force enable

# docker (official one-liner is fine for a single-purpose VPS)
curl -fsSL https://get.docker.com | sh
usermod -aG docker starship
```

Log out, log back in as `starship` from now on:
```bash
ssh -i ~/.ssh/starship_vps starship@<vps-ip>
```

---

## Step 2 — First deploy + smoke test

On the VPS as the `starship` user:

```bash
# clone over HTTPS so the deploy doesn't depend on a GH SSH key yet
git clone https://github.com/<user>/Starship.git
cd Starship

# generate compose .env from the template
cp .env.example .env
chmod 600 .env

# fill in:
#   DISCORD_TOKEN=...           (Discord developer portal)
#   DISCORD_APPLICATION_ID=...
#   POSTGRES_PASSWORD=$(openssl rand -base64 33 | tr -d '/+=\n' | cut -c1-40)
#   REALMEYE_USER_AGENT=...
nano .env
```

Optional but recommended — restore a dev DB dump so you don't re-do
emojis + templates:
```bash
# on WSL
pg_dump starship > starship.sql
scp starship.sql vps:~/Starship/

# on VPS — restore into the (still-empty) compose postgres volume
docker compose up -d postgres
docker compose exec -T postgres psql -U starship -d starship < starship.sql
```

Then bring up the bot:
```bash
./deploy.sh
docker compose logs -f bot
```

Smoke test in a real Discord guild:
- `/setup` opens the wizard.
- A headcount → reactions → run flow completes end-to-end.
- `/verify` works (RealmEye scrape + role assignment).
- Bot survives `docker compose restart bot` (orphan sweep cleans up).

If anything fails here, fix it before moving on. The next steps assume a
working baseline.

---

## Step 3 — Postgres backup cron

Nightly `pg_dump` from inside the postgres compose service, gzipped to a
user-owned directory, 14-day rotation. Script lives at
[`scripts/backup.sh`](scripts/backup.sh) and is committed in the repo —
so a `git pull` on the VPS gives you the latest version.

The script:
- Uses the staged-tmp + rename pattern, so a broken pg_dump never
  surfaces as the latest backup.
- Aborts with a non-zero exit if the gzipped dump is under 10 KB — that
  threshold catches the case where pg_dump errors out before producing
  data (schema-only dumps land around 7 KB, so anything smaller is
  broken).
- Rotates anything older than `KEEP_DAYS` (default 14).

Install on the VPS (one-time, as the `starship` user):

```bash
mkdir -p ~/backups
chmod 700 ~/backups

# add to user crontab
crontab -e
# paste:
# 17 3 * * * /home/starship/Starship/scripts/backup.sh >>/home/starship/backups/backup.log 2>&1
```

Verify with a one-shot manual run:
```bash
~/Starship/scripts/backup.sh
ls -lh ~/backups/starship/
```

**Restoration drill** — run this once now, then again whenever the
schema changes meaningfully. Tests that the dump is real and the
restore path actually works:

```bash
LATEST=$(ls -t ~/backups/starship/starship-*.sql.gz | head -1)

docker compose exec -T postgres psql -U starship -d postgres \
    -c 'CREATE DATABASE starship_restore_test;'

gunzip -c "$LATEST" | docker compose exec -T postgres \
    psql -U starship -d starship_restore_test

# sanity check
docker compose exec -T postgres psql -U starship -d starship_restore_test \
    -c 'SELECT count(*) FROM guilds;'

docker compose exec -T postgres psql -U starship -d postgres \
    -c 'DROP DATABASE starship_restore_test;'
```

Off-site copy is a separate question — for a hobby bot the host-local
backup is enough, but if the VPS dies you lose at most 24h plus
whatever's in flight. Cheap upgrade is `rclone` to a Backblaze B2
bucket nightly (~$0/mo at this volume). Defer until we feel the pain.

---

## Step 4 — Tracing → Discord DM

Live in [`src/services/error_dm.rs`](src/services/error_dm.rs). Design
notes (from when this was a proposal — kept here so the rationale is
co-located with the configuration):

- A tracing `Layer` filters `WARN`+ events and forwards them through a
  bounded `mpsc` channel (capacity 256). Backpressure drops events
  rather than blocking the tracing call site.
- A background task drains the channel, batches events for 30 s,
  dedups by `(level, target, message)` keeping a count
  (`(x12) failed to fetch message …`), and DMs each recipient via the
  bot's existing Discord HTTP client.
- Recursion guard: events whose target starts with `serenity::` or
  `starship::services::error_dm` are dropped before they enter the
  channel. Without this, a Discord outage would loop — every failed
  DM logs an error, which the layer would try to DM, etc.
- The dispatch loop is spawned from `main::run_bot` after the
  `serenity::Client` is built (its HTTP client is reusable and works
  without a live gateway connection), so the layer is alive before
  `client.start()`.

**Configure recipients:** set `ERROR_DM_USER_IDS` in `.env` to a
comma-separated list of Discord user IDs:

```
ERROR_DM_USER_IDS=123456789012345678
```

To find your user ID: turn on Discord Developer Mode (User Settings →
Advanced → Developer Mode), right-click your name in any chat → Copy
User ID.

Empty `ERROR_DM_USER_IDS` = the dispatch task isn't spawned, the
channel closes, and the in-process layer becomes a no-op. Useful for
dev so iteration noise doesn't ping you.

**Verify it's wired** — once in prod, force a WARN to confirm the
pipeline:

```bash
ssh starship@<vps>
cd ~/Starship
docker compose exec -T postgres psql -U starship -d starship \
    -c 'SELECT * FROM nonexistent_table;' 2>/dev/null || true
# Some operations log WARN on failure; tail the bot logs to confirm
# the layer fired, then check your DMs (~30s batch window).
docker compose logs --since=2m bot | grep -E 'WARN|ERROR'
```

Why DM and not a Discord webhook URL: the bot already has an
authenticated HTTP client, the recipient list is just user IDs,
there's nothing to leak/rotate. Webhooks add a second secret.

Why not a dedicated channel: DMs are unmissable; a channel can be muted
or buried. If the volume gets noisy later we pivot to a
`#starship-alerts` channel (same layer, different sink) — but that's
a future change, not a launch-day one.

---

## Step 5 — GitHub Actions CI/CD

Trigger: push to `main`. Pipeline:
1. Build + lint + test on a GitHub-hosted runner (`SQLX_OFFLINE=true`,
   no DB needed).
2. SSH to the VPS as the `starship` user and run `deploy.sh`.
3. Post deploy-start / deploy-done / deploy-failed notifications to a
   Discord channel via webhook.

The workflow lives at
[`.github/workflows/deploy.yml`](.github/workflows/deploy.yml). Builds
happen *on the VPS* under Docker (the Compose layer cache is what
matters for fast redeploys); Actions just orchestrates.

### 5.1 VPS side — deploy key

Generate a dedicated SSH key for GitHub Actions on the VPS. Authorise it
to run *only* `deploy.sh`, so the key is useless if leaked.

```bash
# on the VPS, as the starship user
ssh-keygen -t ed25519 -C "github-actions" -f ~/.ssh/gh_deploy -N ""
```

Edit `~/.ssh/authorized_keys` and add the deploy key as a new line,
prefixed with the lockdown options:

```
command="/home/starship/Starship/deploy.sh",no-port-forwarding,no-X11-forwarding,no-agent-forwarding,no-pty ssh-ed25519 AAAA…<paste contents of ~/.ssh/gh_deploy.pub>… github-actions
```

The `command=` directive overrides whatever the SSH client tries to
run. Even an interactive `ssh` invocation runs only `deploy.sh` and
exits.

Then dump the private key for GH Secrets, and the host fingerprint for
known-hosts pinning:

```bash
cat ~/.ssh/gh_deploy            # private key — paste into VPS_SSH_KEY
ssh-keyscan -t ed25519 $(hostname -I | awk '{print $1}')   # paste into VPS_KNOWN_HOSTS
```

### 5.2 Discord webhook

Pick (or create) a channel for deploy notifications. In Discord:
**Channel Settings → Integrations → Webhooks → New Webhook**, give it a
name (e.g. `Starship Deploys`), copy the webhook URL.

The URL is the secret — anyone with it can post to that channel as
that webhook. Treat it like a token. Rotate via the same UI if it leaks.

### 5.3 GitHub secrets

Repo → Settings → Secrets and variables → Actions → New repository
secret. Add:

| Secret | Value |
|--------|-------|
| `VPS_HOST` | VPS IP or hostname |
| `VPS_USER` | `starship` |
| `VPS_SSH_KEY` | full contents of `~/.ssh/gh_deploy` (private key, multi-line) |
| `VPS_KNOWN_HOSTS` | `ssh-keyscan` output from 5.1 |
| `DISCORD_DEPLOY_WEBHOOK_URL` | from 5.2 (optional — workflow no-ops the notify steps if empty) |

### 5.4 Verify

Push a no-op commit (or use the **Actions** tab → **deploy** workflow →
**Run workflow** button). Watch the run; you should see, in order:

- `:construction: Deploying ABC1234 to prod (bot will restart in ~30s)…`
  in the Discord channel.
- The workflow's `Trigger deploy` step succeeding (it'll show the
  deploy.sh output streamed back).
- `:white_check_mark: Deployed ABC1234 to prod — bot is back up`.

Failure (build error, SSH timeout, etc.) sends a `:x:` message with a
direct link to the failed run.

### 5.5 Migration concern

`sqlx::migrate!` runs on bot startup, so the deploy is a single
`docker compose up -d --build` and forward-only migrations are
self-applying. Backwards-incompatible schema changes (drop a column the
old binary still reads) need a 2-deploy dance: deploy code that doesn't
read the column, *then* deploy the migration. Worth flagging in commit
messages but not worth automating until we hit one.

---

## Credentials needed today

- Discord: `DISCORD_TOKEN`, `DISCORD_APPLICATION_ID`
  (https://discord.com/developers/applications). Privileged intents
  Message Content + Server Members must be ON.
- Hetzner: account (with payment + ID verified).
- Local: `~/.ssh/starship_vps` keypair (we generate this in 1.2).
- Bot: `REALMEYE_USER_AGENT` — the existing dev value is fine, just
  contactable.
- Generated on VPS: `POSTGRES_PASSWORD` (the openssl one-liner in 2).

GH Actions secrets are generated in step 5; not blocking initial deploy.

---

## Progress

Append-only — date + one-line note when each step lands.
