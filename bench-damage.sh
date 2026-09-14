#!/usr/bin/env bash
# A/B the damage path. Run from a TTY or a quiet terminal, with music playing.
#
# One binary, one variable: CAVAWALL_NO_DAMAGE=1 restores full-surface damage.
# Comparing two builds would measure the builds as well as the change.
#
# fullscreen-watch is stopped first. It restarts cavawall on `workspace` and
# `focusedmon` events, so switching to a clean workspace mid-run would kill the
# instance being measured and silently replace it with another.
set -u
BIN="$HOME/.local/bin/cavawall"
SECS=${SECS:-30}
REPS=${REPS:-3}
H=$(pgrep -x Hyprland | head -1) || { echo "no Hyprland"; exit 1; }
HZ=$(getconf CLK_TCK)
ticks() { awk '{print $14+$15}' /proc/$H/stat; }

echo ":: stopping fullscreen-watch and any running cavawall"
pkill -x fullscreen-watch 2>/dev/null
pkill -x cavawall 2>/dev/null
sleep 1

log=$(mktemp)
run() { # $1 = 0 (damage on) or 1 (damage off)
  CAVAWALL_DEBUG=1 CAVAWALL_NO_DAMAGE=$1 "$BIN" >"$log" 2>&1 &
  local pid=$!
  sleep 4                        # configure, place, start drawing
  local a=$(ticks); sleep "$SECS"; local b=$(ticks)
  kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
  # Parked frames commit nothing, so a parked window measures silence, not damage.
  local parked=$(grep -c parking "$log")
  awk -v a=$a -v b=$b -v hz=$HZ -v s=$SECS -v p=$parked \
      'BEGIN{printf "%.2f%%%s", 100*(b-a)/hz/s, (p>0 ? "  <- PARKED, music stopped" : "")}'
}

echo ":: ${SECS}s windows, $REPS interleaved reps - keep music playing"
for r in $(seq 1 "$REPS"); do
  printf "  rep%s  damage ON   %s\n" "$r" "$(run 0)"
  printf "  rep%s  damage OFF  %s\n" "$r" "$(run 1)"
done
rm -f "$log"

echo ":: restoring"
setsid --fork "$HOME/.local/bin/fullscreen-watch" >/dev/null 2>&1
setsid --fork "$HOME/.local/bin/cavawall-launch"  >/dev/null 2>&1
echo ":: done - fullscreen-watch and cavawall restarted"
