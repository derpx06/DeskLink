#!/bin/bash
# Developer only: screenshots of the UI kit's component sheet (the `ui-kit`
# e2e verb of a debug build), light and dark, into OUTDIR (default
# ~/.cache/desklink-ui/desk-redesign). Runs the debug binary in an empty
# network namespace on a private Xvfb (:57) with a private D-Bus and a
# throwaway HOME: never the real session, config or LAN. The Xvfb is
# started with the inherited descriptors above 2 closed (a `flock` lock on
# one would otherwise outlive this script) and is killed on the way out.
# usage: ui-kit-shot.sh [OUTDIR] [TAG]   (env: SHOOT_BIN)
OUT=$(readlink -f "${1:-$HOME/.cache/desklink-ui/desk-redesign}"); TAG=${2:-1100}
mkdir -p "$OUT"
export SHOOT_OUT="$OUT" SHOOT_TAG="$TAG"
export SHOOT_BIN=${SHOOT_BIN:-$HOME/.cache/desklink-work/wt-target-deskui1/debug/desklink}
exec unshare -Urn bash -c '
ip link set lo up
H=$(mktemp -d)
XPID=
trap "rm -rf $H; [ -n \"$XPID\" ] && kill $XPID 2>/dev/null" EXIT
mkdir -p $H/run $H/state $H/config/gtk-4.0 $H/data; chmod 700 $H/run
Xvfb :57 -screen 0 1500x1800x24 >/dev/null 2>&1 3>&- 4>&- 5>&- 6>&- 7>&- 8>&- 9>&- &
XPID=$!
sleep 1
export DISPLAY=:57 GDK_BACKEND=x11 HOME=$H XDG_RUNTIME_DIR=$H/run XDG_STATE_HOME=$H/state XDG_CONFIG_HOME=$H/config XDG_DATA_HOME=$H/data XDG_CACHE_HOME=$H/cache
unset WAYLAND_DISPLAY
dbus-run-session -- bash -c "
  verb() { gdbus call --session --dest derx06.desklink.com --object-path /derx06/desklink/com/window/1 --method org.gtk.Actions.Activate e2e \"[<\\\"\$1\\\">]\" {} >/dev/null; }
  \$SHOOT_BIN >\$SHOOT_OUT/app.log 2>&1 &
  APP=\$!
  for i in \$(seq 40); do gdbus call --session --dest derx06.desklink.com --object-path /derx06/desklink/com/window/1 --method org.gtk.Actions.List >/dev/null 2>&1 && break; sleep 0.5; done
  sleep 1
  verb \"window-size 400 300\"
  verb \"ui-kit 1100 1500\"
  sleep 2
  for scheme in light dark; do
    verb \"color-scheme \$scheme\"; sleep 1.5
    verb \"screenshot \$SHOOT_OUT/kit-\$scheme-\$SHOOT_TAG.png\"; sleep 1
    mv \$SHOOT_OUT/kit-\$scheme-\$SHOOT_TAG-ui-kit.png \$SHOOT_OUT/kit-\$scheme-\$SHOOT_TAG.png
  done
  kill \$APP 2>/dev/null
"
'
