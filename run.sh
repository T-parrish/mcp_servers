#!/bin/sh
# Launcher for MCP clients (Claude Code, Claude Desktop, ...):
#
#   ./run.sh spotify
#   ./run.sh bandcamp
#
# Those clients let you set a command but not a working directory, and `.env` is
# loaded relative to the process's cwd — so run from the workspace root, then
# exec the requested server. Keeps `.env` the single place configuration lives.
#
# Build first: cargo build --release
set -e
cd "$(dirname "$0")"

case "$1" in
  spotify | bandcamp) server="$1" ;;
  *)
    echo "usage: $(basename "$0") {spotify|bandcamp}" >&2
    exit 64 # EX_USAGE
    ;;
esac
shift

binary="target/release/${server}_mcp_server"
if [ ! -x "$binary" ]; then
  echo "$binary not built — run: cargo build --release" >&2
  exit 69 # EX_UNAVAILABLE
fi
exec "$binary" "$@"
