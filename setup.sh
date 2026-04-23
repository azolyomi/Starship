#!/usr/bin/env bash
# Starship setup script.
# Works on both WSL (dev) and a vanilla Ubuntu VPS (prod) — identical steps.
#
# Usage:
#   ./setup.sh                       # fresh install
#   ./setup.sh --restore <dump_file> # fresh install + restore DB dump
#
# Idempotent: re-running is safe. It skips work that's already done.
# Never prints the generated DB password to stdout; it only lands in .env
# (mode 600).

set -euo pipefail

# ---------------------------------------------------------------------------
# Args
# ---------------------------------------------------------------------------
RESTORE_DUMP=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --restore)
            RESTORE_DUMP="${2:?--restore requires a dump file path}"
            shift 2
            ;;
        -h|--help)
            sed -n '2,11p' "$0"
            exit 0
            ;;
        *)
            echo "unknown arg: $1" >&2
            exit 2
            ;;
    esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"

log() { printf '\033[1;36m[setup]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[setup]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[setup]\033[0m %s\n' "$*" >&2; exit 1; }

require_linux() {
    [[ "$(uname -s)" == "Linux" ]] || die "setup.sh only supports Linux (WSL or a Linux server)."
}

# ---------------------------------------------------------------------------
# Rust / cargo
# ---------------------------------------------------------------------------
install_rust() {
    if command -v cargo >/dev/null 2>&1; then
        log "rust already installed ($(rustc --version))"
        return
    fi
    log "installing rustup (no modification of shell profile beyond rustup's default)..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
}

# ---------------------------------------------------------------------------
# PostgreSQL
# ---------------------------------------------------------------------------
install_postgres() {
    if command -v psql >/dev/null 2>&1; then
        log "postgres client already installed"
    else
        log "installing postgresql..."
        sudo apt-get update
        sudo apt-get install -y postgresql postgresql-contrib
    fi

    # WSL: systemd may or may not run. Fall back to pg_ctlcluster / service.
    if command -v systemctl >/dev/null 2>&1 && systemctl list-units --type=service --all 2>/dev/null | grep -q postgresql; then
        sudo systemctl enable --now postgresql || true
    elif command -v service >/dev/null 2>&1; then
        sudo service postgresql start || true
    fi

    # Wait for postgres to accept connections (up to 15s).
    for _ in $(seq 1 15); do
        if sudo -u postgres psql -tAc 'select 1' >/dev/null 2>&1; then
            return
        fi
        sleep 1
    done
    die "postgres did not accept connections within 15s"
}

# ---------------------------------------------------------------------------
# DB user + database
# ---------------------------------------------------------------------------
DB_USER="starship"
DB_NAME="starship"
DB_HOST="localhost"
DB_PORT="5432"

ensure_db() {
    local password
    # Re-use an existing password from .env if present; otherwise generate.
    if [[ -f .env ]] && grep -q '^DATABASE_URL=postgres://' .env; then
        password="$(sed -n 's#^DATABASE_URL=postgres://[^:]*:\([^@]*\)@.*#\1#p' .env)"
        # URL-decode (we only need to handle %XX for safety)
        password="$(printf '%b' "${password//%/\\x}")"
        log "reusing existing DB password from .env"
    else
        password="$(openssl rand -base64 33 | tr -d '/+=\n' | cut -c1-40)"
        log "generated new DB password (40 chars, stored in .env only)"
    fi

    # Create role if missing.
    if sudo -u postgres psql -tAc "select 1 from pg_roles where rolname='$DB_USER'" | grep -q 1; then
        log "role $DB_USER already exists — updating password"
        sudo -u postgres psql -c "alter role \"$DB_USER\" with login password '$password';" >/dev/null
    else
        log "creating role $DB_USER"
        sudo -u postgres psql -c "create role \"$DB_USER\" with login password '$password';" >/dev/null
    fi

    # Create database if missing.
    if sudo -u postgres psql -tAc "select 1 from pg_database where datname='$DB_NAME'" | grep -q 1; then
        log "database $DB_NAME already exists"
    else
        log "creating database $DB_NAME owned by $DB_USER"
        sudo -u postgres createdb -O "$DB_USER" "$DB_NAME"
    fi

    # Grant schema usage (needed on PG 15+ where public is not writable by default).
    sudo -u postgres psql -d "$DB_NAME" -c "grant all on schema public to \"$DB_USER\";" >/dev/null

    # URL-encode the password minimally (@ : / ? # [ ] need escaping).
    local pw_enc
    pw_enc="$(python3 -c 'import sys,urllib.parse;print(urllib.parse.quote(sys.argv[1], safe=""))' "$password" 2>/dev/null \
        || printf '%s' "$password" | sed 's/@/%40/g;s/:/%3A/g;s#/#%2F#g;s/?/%3F/g;s/#/%23/g')"

    DATABASE_URL="postgres://$DB_USER:$pw_enc@$DB_HOST:$DB_PORT/$DB_NAME"
    export DATABASE_URL
}

# ---------------------------------------------------------------------------
# .env
# ---------------------------------------------------------------------------
ensure_env_file() {
    if [[ ! -f .env ]]; then
        log "creating .env from .env.example"
        cp .env.example .env
    fi
    chmod 600 .env

    # Write DATABASE_URL into .env (replace the placeholder line).
    # Use python for safe replacement (avoids sed escaping issues with URL chars).
    python3 - "$DATABASE_URL" <<'PY'
import os, sys, pathlib
url = sys.argv[1]
path = pathlib.Path(".env")
lines = path.read_text().splitlines()
out = []
replaced = False
for ln in lines:
    if ln.startswith("DATABASE_URL="):
        out.append(f"DATABASE_URL={url}")
        replaced = True
    else:
        out.append(ln)
if not replaced:
    out.append(f"DATABASE_URL={url}")
path.write_text("\n".join(out) + "\n")
PY
    log "wrote DATABASE_URL to .env (mode 600)"
}

# ---------------------------------------------------------------------------
# sqlx-cli + migrations
# ---------------------------------------------------------------------------
install_sqlx_cli() {
    if command -v sqlx >/dev/null 2>&1; then
        log "sqlx-cli already installed ($(sqlx --version))"
        return
    fi
    log "installing sqlx-cli (postgres only, no default features)..."
    cargo install sqlx-cli --no-default-features --features rustls,postgres --locked
}

run_migrations() {
    log "running sqlx migrations..."
    sqlx migrate run --source migrations
}

# ---------------------------------------------------------------------------
# Optional DB restore
# ---------------------------------------------------------------------------
restore_dump() {
    local dump="$1"
    [[ -f "$dump" ]] || die "dump file not found: $dump"
    log "restoring DB dump from $dump (this drops existing schema in $DB_NAME)"
    # Terminate existing connections to the DB, then drop+recreate.
    sudo -u postgres psql -c "select pg_terminate_backend(pid) from pg_stat_activity where datname='$DB_NAME' and pid <> pg_backend_pid();" >/dev/null || true
    sudo -u postgres dropdb --if-exists "$DB_NAME"
    sudo -u postgres createdb -O "$DB_USER" "$DB_NAME"
    sudo -u postgres psql -d "$DB_NAME" -c "grant all on schema public to \"$DB_USER\";" >/dev/null

    # Restore as the starship user via local TCP (psql).
    PGPASSWORD_FILE=/dev/null \
    PGPASSWORD="$(sed -n 's#^DATABASE_URL=postgres://[^:]*:\([^@]*\)@.*#\1#p' .env | python3 -c 'import sys,urllib.parse;print(urllib.parse.unquote(sys.stdin.read().strip()))')" \
        psql -h "$DB_HOST" -p "$DB_PORT" -U "$DB_USER" -d "$DB_NAME" -v ON_ERROR_STOP=1 -f "$dump"
    log "restore complete"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
main() {
    require_linux
    install_rust
    install_postgres
    ensure_db
    ensure_env_file
    install_sqlx_cli

    if [[ -n "$RESTORE_DUMP" ]]; then
        restore_dump "$RESTORE_DUMP"
    else
        run_migrations
    fi

    log "setup complete."
    log "next: edit .env to set DISCORD_TOKEN, DISCORD_APPLICATION_ID, DISCORD_TEST_GUILD_ID, then run \`cargo run\`"
}

main "$@"
