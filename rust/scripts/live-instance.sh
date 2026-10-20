#!/bin/sh
# Run a second machine-service instance for live tests (docs/arch/test-layers.md,
# TL-4), beside the installed daemon and without touching it.
#
# The instance runs detached in its own session, so stopping the shell or
# task that started it cannot signal the `tart run` processes of its VMs.
# `stop` signals only the daemon's own PID; its VMs keep running, and the
# next `start` adopts their leases from the state directory.
#
# Usage: scripts/live-instance.sh start <guest-link> | stop | status
# Environment: LIVE_PORT (default 6250); LIVE_BIN (default target/live), the
# directory holding `machine-service` and the agents from build-guest-agents.sh.
# Each port keeps its state, logs and PID under LIVE_BIN/<port>, so two
# instances, one per guest link, can run at once.
set -eu

cd "$(dirname "$0")/.."
port="${LIVE_PORT:-6250}"
bin="$(cd "${LIVE_BIN:-target/live}" && pwd)"
dir="$bin/$port"
mkdir -p "$dir"
pidfile="$dir/daemon.pid"

running() {
    [ -f "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null
}

case "${1:-}" in
start)
    link="${2:?usage: live-instance.sh start <guest-link>}"
    if running; then
        echo "already running: pid $(cat "$pidfile")" >&2
        exit 1
    fi
    mkdir -p "$dir/state"
    VM_SERVICE_PORT="$port" VM_SERVICE_HOST=127.0.0.1 \
        VM_SERVICE_STATE="$dir/state" VM_SERVICE_GUEST_LINK="$link" \
        nohup perl -MPOSIX -e 'POSIX::setsid(); exec @ARGV or die "exec: $!"' \
        "$bin/machine-service" >>"$dir/daemon.out" 2>>"$dir/daemon.err" </dev/null &
    echo $! >"$pidfile"
    tries=0
    until curl -fsS -m 2 "http://127.0.0.1:$port/health" >/dev/null 2>&1; do
        tries=$((tries + 1))
        if [ "$tries" -ge 40 ] || ! running; then
            echo "daemon did not answer on port $port; see $dir/daemon.err" >&2
            exit 1
        fi
        sleep 0.25
    done
    echo "started: pid $(cat "$pidfile"), port $port, guest link $link"
    ;;
stop)
    if running; then
        pid="$(cat "$pidfile")"
        kill "$pid"
        while kill -0 "$pid" 2>/dev/null; do sleep 0.1; done
        echo "stopped: pid $pid"
    fi
    rm -f "$pidfile"
    ;;
status)
    if running; then echo "running: pid $(cat "$pidfile")"; else echo "stopped"; fi
    ;;
*)
    echo "usage: live-instance.sh start <guest-link> | stop | status" >&2
    exit 2
    ;;
esac
