# AGENTS.md

Rules for anyone (human or agent) changing Hearth. The design is in [docs/spec.md](docs/spec.md); decisions are in [docs/decisions/](docs/decisions/). Code that must differ from the spec is reported as a deviation, not silently shipped.

## Design rules the code must keep

### Privacy (spec §6). These are never traded for convenience.

1. **No telemetry, analytics, crash reporting or usage pings.** No code for them, and no dependency that provides them.
2. **Hearth's own processes make no network requests.** Rust core crates take no HTTP or WebSocket client crate. UI code uses no `fetch`, `XMLHttpRequest`, `WebSocket` or `EventSource`, and loads no remote origin. The webview CSP stays closed. Network traffic comes only from `claude`, `codex`, `git` and `gh`, which the user runs and signs in to.
3. **Never read agent credential or config files**: `~/.claude*`, `~/.codex/`, `~/.config/gh`, `~/.ssh`, keychains. All core file access goes through the file-access guard (an allowlist of project dirs, their worktrees and the Hearth data dir).
4. **No identifiers.** No install ID, device ID, account ID or fingerprint is generated or stored.
5. **All data is local** in the Hearth data dir and deletable from Settings. Diagnostics are exported only by the user, to a local file they see first.
6. **Nothing is pushed, committed or opened** on a remote without an explicit user action.

Banned dependencies (direct or transitive; CI fails on a match; the authoritative list is in `scripts/privacy-gates.sh`, keep this copy in sync): `posthog*`, `@sentry/*`, `sentry`, `segment`, `@segment/*`, `mixpanel*`, `amplitude*`, `@amplitude/*`, `@axiomhq/*`, `@datadog/*`, `dd-trace`, `@opentelemetry/exporter-*`, `logrocket`, `fullstory`, `bugsnag`, `rollbar`, `@vercel/analytics`; in Rust core crates also `reqwest`, `hyper` (client), `ureq`, `surf`, `isahc`, `sentry`.

### Agents (spec §4.3)

7. Agents are reached only through an adapter that implements the shared `AgentAdapter` interface and emits Hearth's normalized `AgentEvent`s. No agent-specific branches outside its adapter.
8. The Hearth transcript is the source of truth; agent sessions are disposable.
9. Model lists come from the installed agents, never from a remote manifest.

### Performance (spec §7)

10. Streaming deltas are coalesced and applied once per animation frame.
11. Long lists (transcript, file tree, diff) are virtualized.
12. Diffing, syntax highlighting and markdown for large inputs run in Rust or a Web Worker, never on the UI thread.
13. A change on the transcript, composer or diff path runs the benchmark suite and stays inside the budgets. A dropped frame in the streaming benchmark is a bug.

## Verification

Each one runs once its toolchain exists in the repo:

- `git diff --check`
- `pnpm install --frozen-lockfile`, then `pnpm -r lint`, `pnpm -r typecheck` and `pnpm -r test`
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace`
- `scripts/privacy-gates.sh` (dependency ban list, hard-coded host grep, egress test)

## Test rules

1. Tests use stub agent processes, recorded replay fixtures, and throwaway git repos in a temp dir. No test talks to a real model, GitHub or the network.
2. A test names the defect it catches ("fails if ..."), in its name or a one-line comment.
3. Test behaviour through public interfaces: no tests of file text, of fakes' own logic, of constants or of private internals.
4. Same-shape cases are table-driven.
5. No real time and no sleeps: use injected clocks and explicit settle points.
6. A test must fail when the behaviour it covers is reverted.
7. Replay fixtures are scrubbed of secrets, user names and absolute home paths before commit.

## Commits and pull requests

- Every change is a pull request from a `<type>/<short-kebab-description>` branch (`feat|fix|docs|refactor|test|chore|perf|ci`), merged with a merge commit.
- Conventional-Commit subjects; one concern per commit.
- PR body: what, why, how verified.
