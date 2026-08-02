#!/bin/sh
# Claude Code hook entry. The binary is installed separately (release tarball);
# an unavailable or old binary must never break the session or add hook stderr noise.
if [ -n "${KNIVES_BIN:-}" ] && [ -x "$KNIVES_BIN" ]; then
  bin=$KNIVES_BIN
elif command -v knives >/dev/null 2>&1; then
  bin=knives
else
  exit 0
fi

"$bin" hook claude-code 2>/dev/null
exit 0
