#!/usr/bin/env bash
# Starship deploy script — thin wrapper intended for the VPS.
#
# Usage:
#   ./deploy.sh                  # git pull && compose up -d --build
#   ./deploy.sh --no-pull        # skip git pull (local-only rebuild)
#   ./deploy.sh --logs           # also tail `docker compose logs -f bot`
#                                # after the deploy
#
# Intentionally tiny. The real lifecycle is owned by docker compose's
# `restart: unless-stopped`; this script exists so `git pull && up -d`
# is one command.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"

log() { printf '\033[1;36m[deploy]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[deploy]\033[0m %s\n' "$*" >&2; exit 1; }

PULL=1
FOLLOW_LOGS=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-pull)  PULL=0;        shift ;;
        --logs)     FOLLOW_LOGS=1; shift ;;
        -h|--help)  sed -n '2,13p' "$0"; exit 0 ;;
        *)          die "unknown arg: $1" ;;
    esac
done

command -v docker >/dev/null 2>&1 || die "docker not installed"
docker compose version >/dev/null 2>&1 || die "docker compose plugin not available"

[[ -f .env ]] || die ".env missing — copy .env.example, fill in secrets, then retry"

# Sanity-check that the vars compose interpolates actually exist. Avoids
# silently spinning up a postgres with an empty password.
# shellcheck disable=SC1091
set -a; source .env; set +a
: "${DISCORD_TOKEN:?DISCORD_TOKEN not set in .env}"
: "${DISCORD_APPLICATION_ID:?DISCORD_APPLICATION_ID not set in .env}"
: "${POSTGRES_PASSWORD:?POSTGRES_PASSWORD not set in .env (generate: openssl rand -base64 33 | tr -d '/+=\n' | cut -c1-40)}"

if [[ "$PULL" == "1" ]]; then
    log "git pull"
    git pull --ff-only
fi

log "docker compose up -d --build"
docker compose up -d --build

log "deploy complete. status:"
docker compose ps

if [[ "$FOLLOW_LOGS" == "1" ]]; then
    log "tailing bot logs (ctrl-c to detach; bot keeps running)"
    docker compose logs -f bot
fi
