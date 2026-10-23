#!/bin/sh
set -eu
TARGET="${1:?usage: sh scripts/status_over_ssh.sh user@ssh.instabox.sh}"
case "$TARGET" in *@ssh.instabox.sh) ;; *) exit 2;; esac
ssh -p 2222 "$TARGET" 'cd "$HOME/pg-rss-tsy-live" && docker compose ps && docker compose logs --tail 50'
