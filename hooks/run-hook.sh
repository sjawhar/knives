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

payload=$(cat)
if output=$(printf '%s' "$payload" | "$bin" hook claude-code 2>/dev/null); then
  printf '%s' "$output"
else
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
