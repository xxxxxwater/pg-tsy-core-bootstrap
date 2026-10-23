#!/bin/sh
# Read-only check first; backup + replace only independent heartbeat monitor.
set -eu
if [ "$#" -ge 1 ]; then TARGET="$1"; else
  test -t 0 || { echo 'Provide user@ssh.instabox.sh (needs TTY)' >&2; exit 2; }
  printf 'InstaBox user@ssh.instabox.sh (hidden): ' >&2
  OLD="$(stty -g)"; stty -echo; IFS= read -r TARGET; stty "$OLD"; printf '\n' >&2
fi
case "$TARGET" in *@ssh.instabox.sh) ;; *) echo 'Invalid destination' >&2; exit 2;; esac
ROOT="$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)"
TAR="$(mktemp "${TMPDIR:-/tmp}/monitor-hotfix.XXXXXX")"
trap 'rm -f "$TAR"' EXIT HUP INT TERM
umask 077
tar -czf "$TAR" -C "$ROOT" src/monitor.py scripts/audit_status.py
printf '[1/3] Verify exact InstaBox host and monitor deployment ...\n'
ssh -p 2222 "$TARGET" 'set -eu; test "$(hostname)" = instabox-pg-test; D="$HOME/pg-rss-tsy-live"; test -s "$D/src/monitor.py"; test -s "$D/compose.yaml"; docker compose version; echo "Verified monitor deployment"'
printf '[2/3] Upload hotfix files, take backup, validate Python ...\n'
ssh -p 2222 "$TARGET" 'set -eu; D="$HOME/pg-rss-tsy-live"; mkdir -p "$D/.monitor-staging"; tar -xz -C "$D/.monitor-staging"; python3 -m py_compile "$D/.monitor-staging/src/monitor.py" "$D/.monitor-staging/scripts/audit_status.py"; BACK="$D/src/monitor.py.backup.$(date -u +%Y%m%d%H%M%S)"; cp "$D/src/monitor.py" "$BACK"; cp "$D/.monitor-staging/src/monitor.py" "$D/src/monitor.py"; cp "$D/.monitor-staging/scripts/audit_status.py" "$D/scripts/audit_status.py"; echo "Backup: $BACK"' < "$TAR"
printf '[3/3] Rebuild & restart ONLY health-monitor (not strategy/TSY/RSS) ...\n'
ssh -p 2222 "$TARGET" 'set -eu; cd "$HOME/pg-rss-tsy-live"; docker compose build health-monitor && docker compose up -d --no-deps --force-recreate health-monitor; docker compose ps; sleep 4; docker compose logs --tail=20 health-monitor; python3 scripts/audit_status.py'
echo 'Finished: review whether Telegram notifications have telegram_sent=yes.'
