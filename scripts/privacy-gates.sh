#!/usr/bin/env bash
# Privacy gates. Run from the repo root. Exits non-zero with one message per finding.
# The ban lists below are the single authoritative copy (AGENTS.md and PRIVACY.md point here).
set -euo pipefail

# ---- Authoritative ban lists (shell globs, matched against whole package names) ----
JS_AND_SHARED_BANNED=(
  'posthog*' '@sentry/*' 'sentry' 'segment' '@segment/*' 'mixpanel*' 'amplitude*'
  '@amplitude/*' '@axiomhq/*' '@datadog/*' 'dd-trace' '@opentelemetry/exporter-*'
  'logrocket' 'fullstory' 'bugsnag' 'rollbar' '@vercel/analytics'
)
# Extra names banned in Rust core crates (on top of the shared list above).
RUST_EXTRA_BANNED=('reqwest' 'hyper' 'ureq' 'surf' 'isahc' 'sentry')
# Paths scanned for hard-coded hosts; allowlist is one regex per line (# comments).
HOST_DIRS=(apps crates sidecars packages)
ALLOWLIST="scripts/privacy-allowlist.txt"
# -------------------------------------------------------------------------------------

findings=0
fail() { echo "FINDING: $*" >&2; findings=$((findings + 1)); }

matches_glob() { # name glob...
  local name=$1 g; shift
  for g in "$@"; do
    # shellcheck disable=SC2053
    [[ $name == $g ]] && return 0
  done
  return 1
}

# ---- JS dependencies ----
if [[ -f pnpm-lock.yaml ]]; then
  names=$(grep -oE "^[[:space:]]+['\"]?/?(@[A-Za-z0-9._~-]+/)?[A-Za-z0-9._~-]+(@[^:'\"]*)?['\"]?:" pnpm-lock.yaml \
    | sed -E "s/^[[:space:]]+//; s/:\$//; s/^['\"]//; s/['\"]\$//; s#^/##" \
    | sed -E 's/^(@?[^@]+)@.*$/\1/' | sort -u || true)
  while IFS= read -r n; do
    [[ -z $n ]] && continue
    if matches_glob "$n" "${JS_AND_SHARED_BANNED[@]}"; then
      fail "banned JS dependency '$n' in pnpm-lock.yaml"
    fi
  done <<< "$names"
else
  echo "skipped JS deps: no pnpm-lock.yaml"
fi

# ---- Rust dependencies ----
manifests=()
for d in crates/* apps/desktop/src-tauri; do
  [[ -f $d/Cargo.toml ]] && manifests+=("$d/Cargo.toml")
done
if (( ${#manifests[@]} == 0 )); then
  echo "skipped Rust deps: no Cargo package under crates/ or apps/desktop/src-tauri/"
else
  for m in "${manifests[@]}"; do
    if ! tree=$(cargo tree --offline -e normal --prefix none --manifest-path "$m" 2>&1); then
      fail "cargo tree failed for $m: $(echo "$tree" | head -3 | tr '\n' ' ')"
      continue
    fi
    while IFS= read -r n; do
      [[ -z $n ]] && continue
      if matches_glob "$n" "${JS_AND_SHARED_BANNED[@]}" "${RUST_EXTRA_BANNED[@]}"; then
        fail "banned Rust dependency '$n' in $m"
      fi
    done < <(awk '{print $1}' <<< "$tree" | sort -u)
  done
fi

# ---- Hard-coded hosts ----
allow=()
if [[ -f $ALLOWLIST ]]; then
  while IFS= read -r line; do
    [[ -z $line || $line == \#* ]] && continue
    allow+=("$line")
  done < "$ALLOWLIST"
fi
existing=()
for d in "${HOST_DIRS[@]}"; do [[ -d $d ]] && existing+=("$d"); done
if (( ${#existing[@]} > 0 )); then
  while IFS= read -r f; do
    case $f in
      *node_modules/*|*/target/*|*/fixtures/*|*.md) continue ;;
    esac
    [[ -f $f ]] || continue
    while IFS= read -r hit; do
      [[ -z $hit ]] && continue
      ok=0
      for re in ${allow[@]+"${allow[@]}"}; do
        if [[ $hit =~ $re ]]; then ok=1; break; fi
      done
      (( ok )) || fail "hard-coded host '$hit' in $f (add to $ALLOWLIST only if it is a docs link or local-only)"
    done < <(grep -aoE "https?://[^][:space:]\"'<>)\`;,]+" "$f" || true)
  done < <(git ls-files -- "${existing[@]}")
fi

if (( findings > 0 )); then
  echo "privacy gates: $findings finding(s)" >&2
  exit 1
fi
echo "privacy gates: ok"
