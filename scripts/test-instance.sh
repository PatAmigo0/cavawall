#!/usr/bin/env bash
# Run a throwaway cavawall beside the session's, and always take it down.
#
#   scripts/test-instance.sh <config.toml> [command...]
#
# The instance gets a runtime dir of its own - its own lock, control
# socket and log - so the real one is never touched, and scripts/fake-cava in place of
# cava, so nothing has to play sound. The command runs with these set:
#
#   PID     the test instance
#   CTL     cavawall aimed at it: $CTL status, $CTL reload
#   SILENT  touch it to silence the fake cava, rm it to make noise again
#
# With no command it runs until ctrl-c. The EXIT trap stops the instance and
# its cava however the command ends - an instance left behind is bars drawn on
# someone's screen. CAVAWALL_BIN picks the binary (target/release/cavawall);
# TEST_ENV adds environment, e.g. TEST_ENV=CAVAWALL_DEBUG=1; the log is
# $RT/log, printed on exit when TEST_LOG=1
set -u
here=$(cd "$(dirname "$0")" && pwd)
bin=${CAVAWALL_BIN:-$here/../target/release/cavawall}
bin=$(realpath "$bin")
cfg=$(realpath "$1"); shift
RT=$(mktemp -d "${XDG_RUNTIME_DIR:-/tmp}/cavawall-test.XXXXXX")
chmod 700 "$RT"
mkdir "$RT/bin" && ln -s "$here/fake-cava" "$RT/bin/cava"
export SILENT=$RT/silent
export CTL="env XDG_RUNTIME_DIR=$RT $bin"
display=${WAYLAND_DISPLAY:-wayland-1}
case $display in /*) ;; *) display=${XDG_RUNTIME_DIR:-/run/user/$UID}/$display ;; esac

cleanup() {
    # The command's own children too: a trap that stops the instance but
    # leaves the command running has cleaned up nothing
    if [ -n "${CMD:-}" ]; then pkill -TERM -P "$CMD" 2>/dev/null; kill -TERM "$CMD" 2>/dev/null; fi
    [ -n "${PID:-}" ] && XDG_RUNTIME_DIR=$RT timeout 3 "$bin" stop >/dev/null 2>&1
    sleep 0.3
    for p in $(pgrep -f "^$bin --config $cfg$"); do kill -TERM "$p" 2>/dev/null; done
    sleep 0.3
    for p in $(pgrep -f "^$bin --config $cfg$"); do kill -KILL "$p" 2>/dev/null; done
    for p in $(pgrep -f "$RT/bin/cava"); do kill -KILL "$p" 2>/dev/null; done
    [ "${TEST_LOG:-0}" = 1 ] && cat "$RT/log"
    rm -rf "$RT"
}
trap cleanup EXIT INT TERM

env XDG_RUNTIME_DIR="$RT" CAVAWALL_LOG_DIR="$RT/state" CAVAWALL_HYPR_RUNTIME="${XDG_RUNTIME_DIR:-}" WAYLAND_DISPLAY="$display" PATH="$RT/bin:$PATH" \
    FAKE_CAVA_SILENT="$SILENT" ${TEST_ENV:-} setsid "$bin" --config "$cfg" >"$RT/log" 2>&1 </dev/null &
sleep "${WARMUP:-2}"
PID=$(pgrep -f "^$bin --config $cfg$" | head -1)
export PID
if [ -z "$PID" ]; then
    echo "test-instance: did not start; log:" >&2
    cat "$RT/log" >&2
    exit 1
fi
# In the background and waited for: bash runs a trap only between commands,
# so a foreground command would hold off SIGTERM until it finished on its own
if [ $# -eq 0 ]; then
    echo "test-instance: pid $PID, ctrl-c to stop"
    sleep infinity &
else
    "$@" &
fi
CMD=$!
wait "$CMD"
