#!/usr/bin/env bash
# Egress test: Hearth's own processes open no network connections (PRIVACY.md promise 2).
#
# Usage: tests/privacy/egress-test.sh [--self-test] [--app PATH] [--seconds N]
#   (default)     build the debug app (or use --app: a binary or a .app bundle), launch it with
#                 stubs only (it starts no agents), let it render and idle N seconds (default 10),
#                 and fail if any process belonging to Hearth holds an inet socket.
#   --self-test   build a variant with the test-only cargo feature `egress-selftest`, plant one
#                 deliberate connection per run (one from the app process, one from the WebKit
#                 networking helper) and fail unless the detector catches both.
#
# Mechanism (macOS, no sudo, no Screen Recording permission, works on a GitHub macOS runner):
#   1. The app process runs under `sandbox-exec` with `(deny network-outbound (remote ip))
#      (with send-signal SIGKILL)`. Any attempt to reach any IP, loopback included, kills it, so
#      "the app died before the idle window ended" is a violation. This is enforcement, so a
#      connection shorter than a poll interval cannot slip through.
#   2. The webview's XPC helpers (com.apple.WebKit.WebContent / Networking / GPU) are launched by
#      launchd (parent pid 1), not by the app, and the sandbox does not follow them. They are
#      attributed by snapshotting the WebKit helper pids before launch and taking those that
#      appear after it, and are polled every 0.25 s with `lsof -a -p <pids> -i`, together with
#      the app pid and its descendants. Any inet socket (listening, UDP or connected) is a
#      violation: a clean Hearth holds none. The run fails if no helper is attributed, so the
#      check cannot pass by watching nothing.
#   Rejected: `nettop -L` and `tcpdump` (need root or give no pid attribution for short flows),
#   `launchctl procinfo` (root only), `log show` sandbox reports (delayed, unreliable).
#   Known limit: another app starting its own WebKit helpers during the window would be
#   attributed to Hearth; that fails safe (a false alarm), never a false pass.
#
# Exit: 0 clean, 1 egress detected (or, in --self-test, detector blind), 2 harness error.
set -u -o pipefail
export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
self_test=0 app="" seconds=10
while [ $# -gt 0 ]; do
  case "$1" in
    --self-test) self_test=1 ;;
    --app) app="${2:?--app needs a path}"; shift ;;
    --seconds) seconds="${2:?--seconds needs a number}"; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done
[ "$(uname -s)" = Darwin ] || { echo "egress-test: macOS only (needs sandbox-exec and lsof)" >&2; exit 2; }

work=$(mktemp -d "${TMPDIR:-/tmp}/hearth-egress.XXXXXX")
profile="$work/deny-network.sb"
cat > "$profile" <<'EOF'
(version 1)
(allow default)
(deny network-outbound (remote ip) (with send-signal SIGKILL))
EOF

app_pid="" listener_pid="" new_helpers=""
helper_pids() { ps -axo pid=,command= | grep -E 'com\.apple\.WebKit\.(WebContent|Networking|GPU)' | grep -v grep | awk '{print $1}' | sort -n; }
descendants() { local c; for c in $(pgrep -P "$1" 2>/dev/null); do echo "$c"; descendants "$c"; done; }

cleanup() {
  local p
  for p in $app_pid $new_helpers $listener_pid; do kill "$p" 2>/dev/null; done
  sleep 0.5
  for p in $app_pid $new_helpers $listener_pid; do kill -9 "$p" 2>/dev/null; done
  rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 2' INT TERM

build() { # <target-dir> [cargo feature]; prints the binary path
  local td=$1 feat=${2:-} args=(--debug --no-bundle)
  [ -n "$feat" ] && args+=(--features "$feat")
  (cd "$root" && CARGO_TARGET_DIR="$td" pnpm --filter desktop tauri build "${args[@]}") >&2 || return 1
  echo "$td/debug/hearth-desktop"
}

resolve_bin() { # a binary, or a .app bundle
  if [ -d "$1" ]; then echo "$1/Contents/MacOS/$(ls "$1/Contents/MacOS" | head -1)"; else echo "$1"; fi
}

# run_once <binary> [VAR=value ...]: 0 clean, 1 egress, 2 harness error
run_once() {
  local bin=$1; shift
  local before; before=" $(helper_pids | tr '\n' ' ')"
  env "$@" sandbox-exec -f "$profile" "$bin" > "$work/app.log" 2>&1 &
  app_pid=$!
  local end=$((SECONDS + seconds)) tick=0 findings="" seen_helpers="" p pids hit
  while [ "$SECONDS" -lt "$end" ] || [ "$tick" -lt 4 ]; do
    tick=$((tick + 1))
    if ! kill -0 "$app_pid" 2>/dev/null; then
      wait "$app_pid"; local rc=$?
      echo "FINDING: app exited early (status $rc; 137 = killed by the network-deny rule)"
      tail -n 5 "$work/app.log"
      return 1
    fi
    new_helpers=""
    for p in $(helper_pids); do case "$before" in *" $p "*) ;; *) new_helpers="$new_helpers $p" ;; esac; done
    seen_helpers="$new_helpers"
    pids=$(echo "$app_pid $new_helpers $(descendants "$app_pid")" | tr -s ' ' '\n' | grep . | paste -sd, -)
    hit=$(lsof -a -p "$pids" -i -n -P 2>/dev/null)
    if [ -n "$hit" ]; then
      echo "FINDING: network sockets held by Hearth processes ($pids):"; echo "$hit"
      return 1
    fi
    sleep 0.25
  done
  local names; names=$(for p in $seen_helpers; do ps -o command= -p "$p" 2>/dev/null | sed -E 's#.*/(com\.apple\.WebKit\.[A-Za-z]+)$#\1#'; done | sort | paste -sd, -)
  echo "watched app pid $app_pid and WebKit helpers [$names] ($seen_helpers ) for ${seconds}s"
  case "$names" in *Networking*WebContent*|*WebContent*Networking*) ;; *)
    echo "harness error: WebContent and Networking helpers were not attributed; nothing proves they were watched" >&2
    return 2 ;;
  esac
  return 0
}

stop_app() {
  local p
  for p in $app_pid $new_helpers; do kill "$p" 2>/dev/null; done
  sleep 1
  for p in $app_pid $new_helpers; do kill -9 "$p" 2>/dev/null; done
  wait "$app_pid" 2>/dev/null
  app_pid="" new_helpers=""
}

start_listener() { # holds loopback connections open; sets $addr
  python3 - "$work/port" <<'PY' &
import socket, sys, time
s = socket.socket(); s.bind(("127.0.0.1", 0)); s.listen(8)
open(sys.argv[1], "w").write(str(s.getsockname()[1]))
conns = []
while True:
    conns.append(s.accept()[0])
PY
  listener_pid=$!
  local i=0
  while [ ! -s "$work/port" ] && [ $i -lt 40 ]; do sleep 0.1; i=$((i + 1)); done
  [ -s "$work/port" ] || { echo "harness error: listener did not start" >&2; return 2; }
  addr="127.0.0.1:$(cat "$work/port")"
}

if [ "$self_test" -eq 0 ]; then
  if [ -n "$app" ]; then bin=$(resolve_bin "$app"); else bin=$(build "$root/target") || { echo "build failed" >&2; exit 2; }; fi
  [ -x "$bin" ] || { echo "no executable at $bin" >&2; exit 2; }
  run_once "$bin"; rc=$?
  stop_app
  [ "$rc" -eq 0 ] && echo "egress-test: PASS (no network connections from Hearth processes)"
  exit "$rc"
fi

# ---- self-test: the planted variant must be caught, in the app process and in a helper ----
bin=$(build "$root/target/egress-selftest" egress-selftest) || { echo "planted build failed" >&2; exit 2; }
start_listener || exit 2
fail=0
for mode in "rust=$addr" "webview=http://$addr/"; do
  echo "--- planted: ${mode%%=*}"
  run_once "$bin" "HEARTH_EGRESS_SELFTEST=$mode"; rc=$?
  stop_app
  if [ "$rc" -eq 1 ]; then echo "caught (as required)"; else echo "SELF-TEST FAILED: planted '${mode%%=*}' egress was not caught (rc=$rc)"; fail=1; fi
done
[ "$fail" -eq 0 ] && echo "egress-test --self-test: PASS (both planted connections detected)"
exit "$fail"
