#!/usr/bin/env bash
# Table-driven test for scripts/privacy-gates.sh. Each case builds a throwaway repo.
set -uo pipefail
export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
pass=0; failn=0

# run_case NAME SETUP_FN EXPECT_EXIT EXPECT_LINE
run_case() {
  local name=$1 setup=$2 want=$3 line=$4 dir out rc
  dir=$(mktemp -d)
  mkdir -p "$dir/scripts"
  cp "$ROOT/scripts/privacy-gates.sh" "$ROOT/scripts/privacy-allowlist.txt" "$dir/scripts/"
  (cd "$dir" && git init -q && "$setup")
  local add="git add -A"; [[ $setup == *_untracked ]] && add=":"   # *_untracked cases never stage their files
  out=$(cd "$dir" && $add && bash scripts/privacy-gates.sh 2>&1); rc=$?
  if [[ $rc -eq $want && $out == *"$line"* ]]; then
    echo "PASS: $name"; pass=$((pass + 1))
  else
    echo "FAIL: $name (exit $rc, wanted $want; wanted line '$line')"; echo "$out" | sed 's/^/    /'
    failn=$((failn + 1))
  fi
  rm -rf "$dir"
}

lock() { printf 'lockfileVersion: 9.0\n%s\n' "$1" > pnpm-lock.yaml; }
mkcrate() { # dir name deps-toml
  mkdir -p "$1/src"; : > "$1/src/lib.rs"
  printf '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2021"\n[dependencies]\n%s\n' "$2" "$3" > "$1/Cargo.toml"
}

s_clean() { :; }
s_js_direct() { lock $'importers:\n  .:\n    dependencies:\n      posthog-js:\n        specifier: ^1.0.0\n        version: 1.0.0'; }
s_js_transitive() { lock $'packages:\n  left-pad@1.0.0:\n    resolution: {integrity: x}\nsnapshots:\n  left-pad@1.0.0:\n    dependencies:\n      mixpanel-browser: 2.0.0'; }
s_js_scoped() { lock $'packages:\n  \'@sentry/browser@7.0.0\':\n    resolution: {integrity: x}'; }
s_js_nearmiss() { lock $'packages:\n  postholder@1.0.0:\n    resolution: {integrity: x}'; }
s_rust_banned() {
  mkcrate vendor/reqwest reqwest ''
  mkcrate crates/core core 'reqwest = { path = "../../vendor/reqwest" }'
}
s_rust_ok() {
  mkcrate vendor/serde_lite serde_lite ''
  mkcrate crates/core core 'serde_lite = { path = "../../vendor/serde_lite" }'
}
s_host_bad() { mkdir -p apps/ui; echo 'const u = "https://example.com/x";' > apps/ui/a.ts; }
s_host_csp() { mkdir -p apps/ui; echo '"csp": "connect-src ipc: http://ipc.localhost; img-src data:"' > apps/ui/a.json; }
s_host_semicolon_bad() { mkdir -p apps/ui; echo '"csp": "connect-src https://evil.example; img-src data:"' > apps/ui/a.json; }
s_host_bad_untracked() { mkdir -p apps/ui; echo 'const u = "https://example.com/x";' > apps/ui/new.ts; }
s_host_allowed() { mkdir -p apps/ui; echo "csp: connect-src ipc: http://ipc.localhost" > apps/ui/a.ts; }
s_host_md() { mkdir -p apps/ui; echo 'See https://example.com/docs' > apps/ui/README.md; }

run_case "clean tree passes"                                   s_clean          0 "privacy gates: ok"
run_case "banned direct JS dep fails"                          s_js_direct      1 "banned JS dependency 'posthog-js'"
run_case "banned transitive JS dep fails"                      s_js_transitive  1 "banned JS dependency 'mixpanel-browser'"
run_case "scoped glob @sentry/browser fails"                   s_js_scoped      1 "banned JS dependency '@sentry/browser'"
run_case "near-miss name postholder passes"                    s_js_nearmiss    0 "privacy gates: ok"
run_case "Rust crate depending on local crate reqwest fails"   s_rust_banned    1 "banned Rust dependency 'reqwest'"
run_case "Rust crate with only allowed deps passes"            s_rust_ok        0 "privacy gates: ok"
run_case "hard-coded https://example.com in apps/ fails"       s_host_bad       1 "hard-coded host 'https://example.com/x'"
run_case "allowlisted http://ipc.localhost passes"             s_host_allowed   0 "privacy gates: ok"
run_case "allowlisted host followed by ; in a CSP passes"      s_host_csp       0 "privacy gates: ok"
run_case "remote host followed by ; in a CSP fails"           s_host_semicolon_bad 1 "hard-coded host 'https://evil.example'"
run_case "hard-coded host in a not-yet-added file fails"       s_host_bad_untracked 1 "hard-coded host 'https://example.com/x'"
run_case "URL inside a .md file passes"                        s_host_md        0 "privacy gates: ok"

echo "total: $pass passed, $failn failed"
(( failn == 0 ))
