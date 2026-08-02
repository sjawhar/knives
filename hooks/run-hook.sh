#!/bin/sh
# Claude Code hook entry. The binary is installed separately (release tarball);
# a missing binary must never break the session, so this exits quietly instead.
command -v knives >/dev/null 2>&1 || exit 0
exec knives hook claude-code
