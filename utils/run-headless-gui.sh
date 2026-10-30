#!/usr/bin/env bash
# GPUI needs window-manager events when running inside Xvfb.
openbox --sm-disable &
wm=$!
cleanup() { kill "$wm" 2>/dev/null || true; }
trap cleanup EXIT
"$@"
