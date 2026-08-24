#!/bin/sh
# Launcher for MCP clients (Claude Code, Claude Desktop, ...) and local bring-up:
#
#   ./run.sh spotify                  # exec one server on stdio (what a client runs)
#   ./run.sh bandcamp --telemetry     # ...and ensure the local Grafana/OTLP stack is up
#   ./run.sh both -t                  # background BOTH servers for local dev, + telemetry
#
# MCP clients let you set a command but not a working directory, and `.env` is
# loaded relative to the process's cwd — so run from the workspace root, then
# exec the requested server. Keeps `.env` the single place configuration lives.
#
# `both` is a dev convenience only: MCP over stdio is one client <-> one server on
# a single channel, so two servers cannot share it. `both` backgrounds each with a
# keep-alive on stdin (logs/<server>.log), which is a local bring-up — Claude Code
# still spawns its own process per registration. Ctrl-C stops both.
#
# With --telemetry (-t) the shared Grafana/Tempo/Prometheus stack from
# docker-compose.yml is brought up first (skipped if already running), so spans
# and metrics have somewhere to land (the server still needs
# OTEL_EXPORTER_OTLP_ENDPOINT set, as it is in .env).
#
# Build first: cargo build --release
set -e
cd "$(dirname "$0")"

# Ensure the shared telemetry stack is running, launching it only if it isn't.
start_telemetry() {
  if ! command -v docker >/dev/null 2>&1; then
    echo "run.sh: --telemetry set but docker is not installed; skipping" >&2
    return
  fi
  # `docker compose ps` lists the running `lgtm` service container's id, if any.
  if [ -n "$(docker compose ps --status running --quiet lgtm 2>/dev/null)" ]; then
    echo "run.sh: telemetry stack already running; skipping launch" >&2
  else
    echo "run.sh: starting telemetry stack..." >&2
    # Output to stderr so it never touches stdout (the MCP protocol stream). A
    # failure is non-fatal: the server still starts, export just has nowhere to go.
    docker compose up -d >&2 || echo "run.sh: could not start the telemetry stack" >&2
  fi
}

binary_for() {
  bin="target/release/${1}_mcp_server"
  if [ ! -x "$bin" ]; then
    echo "$bin not built — run: cargo build --release" >&2
    exit 69 # EX_UNAVAILABLE
  fi
  echo "$bin"
}

# Background both servers for local development. Each reads from a FIFO whose
# write end this script holds open, so the stdio transport never sees EOF and the
# server stays up with no client attached. Closing those ends on exit lets both
# shut down cleanly.
run_both() {
  spotify_bin=$(binary_for spotify)
  bandcamp_bin=$(binary_for bandcamp)
  mkdir -p logs
  fifo_dir=$(mktemp -d "${TMPDIR:-/tmp}/mcp_run.XXXXXX")
  mkfifo "$fifo_dir/spotify.in" "$fifo_dir/bandcamp.in"

  cleanup() {
    echo >&2
    echo "run.sh: stopping..." >&2
    exec 3>&- 4>&- || true # EOF the servers' stdin -> clean exit
    kill "$spotify_pid" "$bandcamp_pid" 2>/dev/null || true
    wait "$spotify_pid" "$bandcamp_pid" 2>/dev/null || true
    rm -rf "$fifo_dir"
  }
  trap 'cleanup; exit 0' INT TERM

  "$spotify_bin" <"$fifo_dir/spotify.in" >logs/spotify.log 2>&1 &
  spotify_pid=$!
  exec 3>"$fifo_dir/spotify.in" # hold stdin open (unblocks the reader above)
  "$bandcamp_bin" <"$fifo_dir/bandcamp.in" >logs/bandcamp.log 2>&1 &
  bandcamp_pid=$!
  exec 4>"$fifo_dir/bandcamp.in"

  echo "run.sh: started spotify (pid $spotify_pid) -> logs/spotify.log" >&2
  echo "run.sh: started bandcamp (pid $bandcamp_pid) -> logs/bandcamp.log" >&2
  echo "run.sh: both running; Ctrl-C to stop" >&2

  # Return here if either server exits on its own, then tear the other down too.
  wait "$spotify_pid" "$bandcamp_pid" 2>/dev/null || true
  cleanup
}

server=""
telemetry=0
for arg in "$@"; do
  case "$arg" in
    -t | --telemetry) telemetry=1 ;;
    spotify | bandcamp | both)
      if [ -n "$server" ]; then
        echo "run.sh: target already set to '$server'" >&2
        exit 64
      fi
      server="$arg"
      ;;
    *)
      echo "run.sh: unknown argument '$arg'" >&2
      server=""
      break
      ;;
  esac
done

if [ -z "$server" ]; then
  echo "usage: $(basename "$0") {spotify|bandcamp|both} [--telemetry]" >&2
  exit 64 # EX_USAGE
fi

[ "$telemetry" -eq 1 ] && start_telemetry

if [ "$server" = "both" ]; then
  run_both
else
  binary=$(binary_for "$server")
  exec "$binary"
fi
