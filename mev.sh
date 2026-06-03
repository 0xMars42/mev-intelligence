#!/usr/bin/env bash
#
# mev.sh — pilote la plateforme mev-intelligence (P3).
#
# Usage :
#   ./mev.sh            # = daemon (ingestion + validation, Ctrl+C pour arreter)
#   ./mev.sh daemon     # idem
#   ./mev.sh analyze    # cluster -> classify -> leaderboard (sur la donnee actuelle)
#   ./mev.sh web        # dashboard + API sur http://127.0.0.1:8080
#   ./mev.sh status     # le daemon tourne-t-il ? + dernieres lignes de log
#   ./mev.sh stop       # arrete le daemon
#
# Le daemon tourne en SERVICE systemd user (mev-intelligence.service) :
#  - survit a la fermeture du terminal ET aux invocations `wsl -e` one-shot
#  - Restart=always : relance auto en cas de crash (WHEA, OOM, panic, WS fatal)
#  - linger active : tourne meme sans session ouverte (tant que WSL/la VM tourne)
# `daemon` (re)demarre le service ; `stop`/`status` le pilotent via systemctl.

set -euo pipefail
cd "$(dirname "$0")"                 # se place dans le repo
export PATH="$HOME/.cargo/bin:$PATH" # pour trouver cargo si build necessaire

BIN="./target/release"
DAEMON="$BIN/mev-intelligence"
LOG="$HOME/mev_daemon.log"
SERVICE="mev-intelligence.service"

build_if_needed() {
  if [ ! -x "$DAEMON" ]; then
    echo ">> Binaires release absents — compilation (cargo build --release, -j4)..."
    CARGO_BUILD_JOBS=4 cargo build --release
  fi
}

case "${1:-daemon}" in
  daemon)
    build_if_needed
    echo ">> (Re)demarrage du service $SERVICE..."
    systemctl --user restart "$SERVICE"
    sleep 2
    systemctl --user is-active "$SERVICE" >/dev/null \
      && echo ">> Service ACTIF — log : $LOG" \
      || echo ">> ECHEC demarrage (voir: systemctl --user status $SERVICE)"
    echo "   ./mev.sh status   pour surveiller"
    echo "   ./mev.sh stop     pour arreter"
    ;;

  analyze)
    build_if_needed
    echo "== cluster =========================================="
    "$BIN/cluster"
    echo "== classify ========================================="
    "$BIN/classify"
    echo "== leaderboard ======================================"
    "$BIN/leaderboard"
    echo "== copy-traders + gas wars ========================="
    "$BIN/copytrader"
    ;;

  web)
    build_if_needed
    echo ">> Dashboard sur http://127.0.0.1:8080 (Ctrl+C pour arreter)"
    exec "$BIN/web"
    ;;

  status)
    if systemctl --user is-active "$SERVICE" >/dev/null 2>&1; then
      pid=$(systemctl --user show -p MainPID --value "$SERVICE")
      since=$(systemctl --user show -p ActiveEnterTimestamp --value "$SERVICE")
      echo "daemon : EN COURS (pid $pid, depuis $since)"
    else
      echo "daemon : ARRETE (systemctl --user status $SERVICE pour le detail)"
    fi
    echo "--- dernieres lignes du log ---"
    sed -r 's/\x1b\[[0-9;]*m//g' "$LOG" 2>/dev/null | tail -n 6 || echo "(pas de log)"
    ;;

  stop)
    # Arret propre du service. `stop` n'empeche pas un futur `daemon`/reboot
    # de le relancer (le service reste `enabled`).
    systemctl --user stop "$SERVICE" && echo "service arrete" || echo "service deja arrete"
    ;;

  *)
    echo "Usage: $0 [daemon|analyze|web|status|stop]"
    exit 1
    ;;
esac
