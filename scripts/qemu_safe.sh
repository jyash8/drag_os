#!/bin/bash
# PID-scoped QEMU control. NEVER pkill qemu: the user keeps a separate QEMU in
# another workspace. Lives in the repo because /tmp has been wiped repeatedly
# mid-session, losing these helpers (and any backup pointer) each time.
set -u
PIDFILE=/tmp/dragos-my-qemu.pid
LOGDIR=/tmp/dragos-qemu-logs
mkdir -p "$LOGDIR"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
qemu_pids() { pgrep -f 'qemu-system-x86_64' 2>/dev/null | sort -n; }
case "${1:-}" in
  start) shift
    if ss -tlnp 2>/dev/null | grep -q ':8080 '; then echo "BLOCKED: 8080 in use"; exit 2; fi
    BEFORE=$(qemu_pids | tr '\n' ' ')
    LOG="$LOGDIR/run_$(date +%H%M%S).log"
    cd "$ROOT"
    setsid nohup cargo run -p tools -- run "$@" > "$LOG" 2>&1 < /dev/null &
    sleep 8
    MINE=""
    for p in $(qemu_pids); do case " $BEFORE " in *" $p "*) ;; *) MINE="$MINE $p";; esac; done
    if [ -z "$MINE" ]; then echo "WARN: no qemu appeared; log=$LOG"; tail -3 "$LOG"; exit 3; fi
    echo "$MINE" | tr -d ' ' > "$PIDFILE"; echo "my qemu:$MINE  log=$LOG" ;;
  stop)
    [ -f "$PIDFILE" ] || { echo "no pid recorded; nothing stopped"; exit 0; }
    for p in $(cat "$PIDFILE"); do kill -0 "$p" 2>/dev/null && kill -TERM "$p" 2>/dev/null || echo "pid $p gone"; done
    sleep 2
    for p in $(cat "$PIDFILE"); do kill -0 "$p" 2>/dev/null && kill -9 "$p" 2>/dev/null; done
    rm -f "$PIDFILE"; echo "stopped only my pids" ;;
  status) echo "mine: $(cat "$PIDFILE" 2>/dev/null || echo none)  all: $(qemu_pids | tr '\n' ' ')" ;;
  *) echo "usage: $0 {start|stop|status} [args]"; exit 1;;
esac
