#!/bin/bash
set -e

# Start guacd in the background. -f keeps it foreground-attached to the
# child PID; -L info lowers the log volume vs the legacy `debug`.
guacd -f -L info -l 4822 &
GUACD_PID=$!

# Forward SIGTERM/SIGINT to both processes so docker stop is clean.
trap 'kill -TERM "$GUACD_PID" "$BRIDGE_PID" 2>/dev/null' TERM INT

# The Rust bridge consumes the AES key as its only positional arg
# (matches the legacy `node server.js $GUAC_KEY` invocation).
flowcase-guac "$GUAC_KEY" &
BRIDGE_PID=$!

# Wait on whichever child exits first; surface its status.
wait -n "$GUACD_PID" "$BRIDGE_PID"
status=$?
kill -TERM "$GUACD_PID" "$BRIDGE_PID" 2>/dev/null || true
exit "$status"
