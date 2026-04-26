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

Goal: every WARN+ event from the bot lands in the superadmin's DMs,
without flooding when something repeats.

**Design (proposed — flag any concern before I code it):**

- New `src/services/error_dm.rs` module exposing a `tracing` `Layer` that
  filters `level >= WARN`, formats one event per line, and pushes it into
  an `mpsc` channel.
- A background task drains the channel, batches events for 30s, dedups
  identical message+target pairs (keep a count: `(x12) failed to fetch
  message …`), and DMs each `superadmin` user via the existing serenity
  HTTP client.
- Recursion guard: if the failing event itself originates from the
  Discord HTTP path (target starts with `serenity::` or `error_dm::`),
  drop it. Otherwise a Discord outage would loop forever trying to DM
  about Discord being down.
- Backpressure: bounded channel (capacity 256). If full, drop and
  increment a local counter that gets flushed in the next batch as
  `"(N events dropped due to backpressure)"`.
- Config: `ERROR_DM_USER_IDS` env var, comma-separated. Empty = layer
  is registered but no-op (still useful so dev doesn't DM you).

Why DM and not a Discord webhook URL: the bot already has an
authenticated client, the recipient list is just user IDs, and there's
nothing to leak/rotate. Webhooks add a second secret to manage.

Why not a separate channel: DMs are unmissable; a channel can be muted
or buried. If the volume gets noisy we can pivot to a `#starship-alerts`
channel later — same Layer, different sink.

**Open questions for you:**
- Single recipient (you) or a small list?
- Cap at WARN+ or include INFO for lifecycle events (run start/end)?
  My vote: WARN+ only, lifecycle stays in the log channel (which already
  exists).

---

## Step 5 — GitHub Actions CI/CD

Trigger: push to `main`. Action: SSH to the VPS as a dedicated deploy
user, run `./deploy.sh`. Builds happen *on the VPS* (the Docker layer
cache there is what matters); Actions just orchestrates.

### 5.1 VPS side
```bash
# on the VPS, as starship user
ssh-keygen -t ed25519 -C "github-actions" -f ~/.ssh/gh_deploy -N ""
cat ~/.ssh/gh_deploy.pub >> ~/.ssh/authorized_keys
cat ~/.ssh/gh_deploy           # copy this private key into GH Secrets
```

Restrict the deploy key to running only `deploy.sh` by prefixing the
`authorized_keys` line:
```
command="cd /home/starship/Starship && ./deploy.sh",no-port-forwarding,no-X11-forwarding,no-agent-forwarding ssh-ed25519 AAAA... github-actions
```
That makes the key useless if leaked — it can only run the deploy
script, not get a shell.

### 5.2 GitHub side
Repo → Settings → Secrets and variables → Actions. Add:
- `VPS_HOST` — the IP or hostname.
- `VPS_USER` — `starship`.
- `VPS_SSH_KEY` — the private key from above (multi-line value).
- `VPS_KNOWN_HOSTS` — output of `ssh-keyscan <vps-host>` so we don't
  bypass host-key checking.

### 5.3 Workflow file
`.github/workflows/deploy.yml`:
```yaml
name: deploy
on:
  push:
    branches: [main]
  workflow_dispatch:

concurrency:
  group: deploy-prod
  cancel-in-progress: false

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --check
      - run: cargo clippy --all-targets -- -D warnings
      # cargo test needs a postgres for sqlx — defer until we want it.
      # - run: cargo test

  deploy:
    needs: test
    runs-on: ubuntu-latest
    steps:
      - name: Configure SSH
        run: |
          mkdir -p ~/.ssh
          echo "${{ secrets.VPS_SSH_KEY }}" > ~/.ssh/id_ed25519
          chmod 600 ~/.ssh/id_ed25519
          echo "${{ secrets.VPS_KNOWN_HOSTS }}" > ~/.ssh/known_hosts
      - name: Trigger deploy
        run: ssh ${{ secrets.VPS_USER }}@${{ secrets.VPS_HOST }}
```
Because of the `command="..."` restriction in `authorized_keys`, the
empty `ssh` invocation runs `deploy.sh` server-side. No `cd`, no shell
injection surface.

### 5.4 Migration concern
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
