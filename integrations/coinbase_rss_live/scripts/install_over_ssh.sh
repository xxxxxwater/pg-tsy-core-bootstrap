#!/bin/sh
# Source machine: user's Mac with ssh and tar. Target: existing instabox-pg-test.
set -eu
if [ "$#" -ge 1 ]; then
  TARGET="$1"
else
  [ -t 0 ] || { echo 'Need an interactive terminal or explicit user@ssh.instabox.sh' >&2; exit 2; }
  printf 'Paste InstaBox user@ssh.instabox.sh: ' >&2
  OLD="$(stty -g)"
  stty -echo
  IFS= read -r TARGET
  stty "$OLD"
  printf '\n' >&2
fi
case "$TARGET" in *@ssh.instabox.sh) ;; *) echo 'Invalid destination' >&2; exit 2;; esac
HERE="$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)"
F="$(mktemp "${TMPDIR:-/tmp}/pg-live-upload.XXXXXX")"
trap 'rm -f "$F"' EXIT HUP INT TERM
umask 077
tar -czf "$F" -C "$HERE" Dockerfile compose.yaml .env.example README.md src/ scripts/ tests/ .dockerignore .gitignore
printf '\n[1/4] Verifying target and existing RSS/TSY footprints without touching either...\n'
ssh -p 2222 "$TARGET" 'set -eu; test "$(hostname)" = "instabox-pg-test"; test -d /data/workspace/pg-tsy-core-bootstrap; test -f "$HOME/puregamma-event-collector/data/events.db"; docker compose version; df -h /data; echo "Verified exact target and both originals"'
printf '\n[2/4] Uploading isolated LIVE connector files ...\n'
ssh -p 2222 "$TARGET" 'set -eu; D="$HOME/pg-rss-tsy-live"; mkdir -p "$D" "$D/state" "$D/secrets"; chmod 700 "$D/state" "$D/secrets"; tar -xz -C "$D"; if test ! -f "$D/.env"; then cp "$D/.env.example" "$D/.env"; chmod 600 "$D/.env"; fi; if test ! -e "$D/secrets/coinbase.env"; then : > "$D/secrets/coinbase.env"; echo "Created empty $D/secrets/coinbase.env (0600): add only COINBASE_API_KEY_NAME / COINBASE_API_PRIVATE_KEY before arming" >&2; fi; chmod 600 "$D/secrets/coinbase.env"; grep -q "^COINBASE_ENV_FILE=" "$D/.env" || { echo "COINBASE_ENV_FILE missing from $D/.env; add COINBASE_ENV_FILE=./secrets/coinbase.env (do not reuse the TSY .env)" >&2; exit 2; }; if grep -q "^TSY_ENV_FILE=" "$D/.env"; then echo "Obsolete TSY_ENV_FILE in $D/.env is ignored; remove it" >&2; fi' < "$F"
printf '\n[3/4] Checking source mounts and Compose settings; live is NOT armed by installer...\n'
ssh -p 2222 "$TARGET" 'set -eu; D="$HOME/pg-rss-tsy-live"; cd "$D"; test -f secrets/coinbase.env; docker compose config -q; grep -q "^PG_LIVE_TRADING=false$" .env || { echo "Existing .env is armed or customized (PG_LIVE_TRADING is not false); refusing to rebuild/restart. Disarm or upgrade manually." >&2; exit 3; }; docker compose build'
printf '\n[4/4] Starting independent strategy and local health-monitor containers...\n'
ssh -p 2222 "$TARGET" 'set -eu; cd "$HOME/pg-rss-tsy-live"; docker compose up -d; docker compose ps; docker compose logs --tail 15'
echo 'Installed. Original pg-core is unchanged. Coinbase LIVE remains blocked until dedicated portfolio and explicit risk budget are configured.'
