#!/bin/sh
# Claude Code hook entry. The binary is installed separately (release tarball);
# an unavailable binary must never break the session or add hook stderr noise.
if [ -n "${KNIVES_BIN:-}" ] && [ -x "$KNIVES_BIN" ]; then
  bin=$KNIVES_BIN
elif command -v knives >/dev/null 2>&1; then
  bin=knives
else
  exit 0
fi

# Bound the stdin read where timeout(1) exists (35s > the binary's own 30s
# watchdog): a harness that spawns this hook and abandons it before writing
# stdin would otherwise park this shell in `cat` forever — the same immortal-
# process class that fork-bombed a devbox on 2026-08-25. Platforms without
# timeout(1) rely on the harness's own hook timeout.
if command -v timeout >/dev/null 2>&1; then
  payload=$(timeout 35 cat)
else
  payload=$(cat)
fi

output=$(printf '%s' "$payload" | "$bin" hook claude-code 2>/dev/null)
status=$?
if [ "$status" -eq 0 ]; then
  printf '%s' "$output"
elif [ "$status" -eq 2 ]; then
  # Exit 2 is a clap usage error: the installed binary predates the `hook`
  # subcommand. The watchdog exits 3 (Incomplete) and degrades silently — a
  # slow box is not a stale binary.
  case $payload in
    # Require the event key, then its colon, then SessionStart. A PostToolUse
    # tool_input can contain that exact JSON fragment and cause one harmless
    # extra systemMessage for a stale binary; avoiding it needs JSON parsing.
    *'"hook_event_name"'*':'*'"SessionStart"'*)
      printf '%s\n' '{"systemMessage":"knives: installed binary cannot serve this plugin (needs the hook subcommand). Update knives or set KNIVES_BIN."}'
      ;;
  esac
fi
exit 0
