#!/usr/bin/env bash
# Nightly Postgres backup for the Starship VPS.
#
# Runs pg_dump from inside the postgres compose service, gzips the output,
# rotates dumps older than $KEEP_DAYS, and refuses to publish anything that
# looks too small to be a real dump.
#
# Crontab entry (as the `starship` user):
#   17 3 * * * /home/starship/Starship/scripts/backup.sh \
#               >>/home/starship/backups/backup.log 2>&1
#
# Restoration drill — run by hand once, then again whenever the schema
# changes meaningfully:
#   gunzip -c /home/starship/backups/starship/starship-<ts>.sql.gz \
#     | docker compose exec -T postgres psql -U starship -d postgres \
#       -c 'CREATE DATABASE starship_restore_test;'
#   gunzip -c /home/starship/backups/starship/starship-<ts>.sql.gz \
#     | docker compose exec -T postgres \
#         psql -U starship -d starship_restore_test
#   docker compose exec -T postgres \
#         psql -U starship -d starship_restore_test \
#         -c 'SELECT count(*) FROM guilds;'
#   docker compose exec -T postgres psql -U starship -d postgres \
#         -c 'DROP DATABASE starship_restore_test;'

set -euo pipefail

REPO=/home/starship/Starship
DEST=/home/starship/backups/starship
KEEP_DAYS=14
# Schema-only dumps land around 7 KB, so anything smaller means pg_dump
# aborted before it produced data. Tune up if the schema grows a lot.
MIN_BYTES=10240

mkdir -p "$DEST"
chmod 700 "$DEST"

cd "$REPO"

ts=$(date -u +%Y%m%dT%H%M%SZ)
out="$DEST/starship-$ts.sql.gz"
tmp="$out.tmp"

# Pipe failure (pg_dump erroring) propagates because of `set -o pipefail`.
# Stage to a .tmp file first; only rename to the real path after we've
# checked the size. A broken dump never surfaces as "the latest backup".
trap 'rm -f "$tmp"' ERR

docker compose exec -T postgres \
    pg_dump -U starship -d starship --clean --if-exists \
    | gzip -9 > "$tmp"

size=$(stat -c%s "$tmp")
if [ "$size" -lt "$MIN_BYTES" ]; then
    echo "$(date -u +%FT%TZ) FATAL: backup is only $size bytes — broken dump?" >&2
    rm -f "$tmp"
    exit 1
fi

mv "$tmp" "$out"

# Rotate. -mtime +N matches files older than N*24h.
find "$DEST" -maxdepth 1 -name 'starship-*.sql.gz' -mtime "+$KEEP_DAYS" -delete

echo "$(date -u +%FT%TZ) ok: $out ($size bytes)"
