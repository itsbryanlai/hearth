# Hearth — v1 Spec

> **Hearth**: one local home for all your coding agents.
> Your subscriptions, your machine, your data. Nothing leaves unless you send it.

Status: Draft v0.2 · 2026-11-27

---

## 1. Why this exists

A desktop app that drives the coding agents you already pay for (Claude Code, Codex) from one window, with git worktrees and pull requests built in, is a strong idea. But such a tool sits on top of your code, your prompts, your agent sessions and your agent logins. Tools in this category have been found sending usage analytics by default, keeping copies of raw events on their own servers, and reading agent login files to link a user's accounts and machines.

Hearth is built so that none of that is possible:

| Practice we rule out | Hearth's answer |
|---|---|
| Analytics on by default, hard to turn off | **No analytics code at all.** It isn't switched off by default; it doesn't exist. Enforced in CI (§6.3). |
| Events batched to a vendor-run ingest server | **No ingest, no backend, no servers.** |
| Usage data (models, tokens, project and thread counts, feature use, device) sent off the machine | Usage stats are computed **locally only**, shown to you, never sent. |
| Reading agent login files (`~/.claude*`, `~/.codex/`) to derive and link account IDs | **Never reads agent credential or config files.** Agents authenticate themselves; Hearth only spawns their CLIs. Enforced by a file-access allowlist (§6.2). |
| Remote relays and third-party diagnostics that some clients can't opt out of | **No relay, no remote access, no third-party SaaS** in v1. |
| Privacy promises on a website that can be quietly edited | Privacy guarantees live in `PRIVACY.md` **in the repo** and are tested. Changing one means changing a failing test in public. |

**A tool that holds your code, prompts and agent sessions should not phone home.**

**Private use only.** Hearth is a personal project for its author's own use. It is not distributed, offered or licensed to anyone else (see `LICENSE`). Each user of an agent CLI signs in to that agent under its own terms; Hearth provides no access to any agent.

---

## 2. Is it feasible?

**Yes.** All of the hard parts have official, documented integration points:

- **Codex**: `codex app-server`, a JSON-RPC protocol over stdio. It's what OpenAI's own IDE extension uses. It covers threads, turns, streaming items, approvals, and model selection.
- **Claude Code**: the **Claude Agent SDK** (TypeScript) or the `claude` CLI in headless `stream-json` mode. Both run the user's own `claude` install with its own login, and both handle tool-permission callbacks.
- **Git**: the `git` CLI (worktrees, diff, commit, push). **PRs**: the `gh` CLI with the user's own `gh auth`.

### Risks to resolve early

1. **Protocol churn.** Codex app-server and Claude's stream protocol change often. Mitigation: a thin adapter per agent, recorded replay fixtures, and pinned "known-good CLI version" ranges with a clear warning outside the range.
2. **Performance claims are easy to make and hard to keep.** Mitigation: budgets in CI from M0 (§7).

---

## 3. v1 scope

### In scope
1. **Your subscriptions.** Drive locally installed, already signed-in agents. Hearth never sells, proxies, or sees tokens.
2. **Every agent in one place.** Claude Code and Codex in v1. Change model mid-thread. Hand a task from one agent to another.
3. **macOS desktop app** (Apple Silicon first, Intel best-effort).
4. **Git built in.** Optional worktree per thread, diff review, then commit, push, and open a PR in one action.
5. **Fast.** Measured budgets, enforced in CI.

### Out of scope for v1 (deliberately)
- Remote access, relays, mobile and web clients, accounts, cloud sync (each one is a surface where data can leave the machine).
- Distribution to anyone but the author (§1, private use only).
- Windows and Linux.
- Agents other than Claude Code and Codex (the adapter interface is designed for more: ACP agents, OpenCode, Cursor CLI, Gemini CLI).
- Forges other than GitHub.
- Any updater or update check (update by pulling and rebuilding; see §6.4).

---

## 4. Product & UX

### 4.1 Core concepts

- **Project**: a local git repository the user adds.
- **Thread**: one conversation and unit of work in a project. It has a title, a working directory (repo root or its own worktree), and an ordered list of **turns**.
- **Turn**: one user message, then the agent's streamed response (text, reasoning, tool calls, file edits, command runs, approvals). Each turn records **which agent and model** produced it.
- **Agent session**: the live connection between a thread and one agent process. A thread can have several over its lifetime (after a model or agent switch). The Hearth transcript is the source of truth; agent sessions are disposable.

### 4.2 Main window

```
┌────────────┬──────────────────────────────────────────┬──────────────┐
│ Projects   │  Thread: "Fix flaky auth test"           │  Diff        │
│  ▸ api     │  [Claude · opus] ▾  worktree: hearth/fix… │  ▸ 3 files   │
│    • Fix…  │                                          │  +42 −7      │
│    • Add…  │  ...streamed turns...                    │              │
│  ▸ web     │                                          │ [Commit]     │
│            │  ┌────────────────────────────────────┐  │ [Push]       │
│ + Project  │  │ Message…        (⌘↵)   [Approve ▾] │  │ [Open PR]    │
└────────────┴──┴────────────────────────────────────┴──┴──────────────┘
```

- Keyboard-first: ⌘K command palette, ⌘N new thread, ⌘⇧D toggle diff, ⌘↵ send, ⌘. interrupt.
- Agent/model picker in the thread header; switching is allowed at any turn boundary.
- Approvals appear inline (allow once / allow for this thread / deny), using each agent's own permission model.
- A **status line per thread**: agent, model, permission mode, worktree branch, and running/idle state.
- A **local usage view**: tokens and turns per agent and model, computed from the local DB. It is never sent anywhere.

### 4.3 Agents

**Detection (no credential reads).** On launch, Hearth looks for `claude` and `codex` on `PATH` and in common install locations, then runs `--version`. Auth state is learned only from the agent's own response: if the first turn fails with an auth error, Hearth shows "Run `claude` / `codex login` in your terminal" with a button that opens Terminal. Hearth **never opens** `~/.claude*`, `~/.codex/*`, keychains, or any token store.

**Adapter interface** (one per agent, same shape for future agents):

```ts
interface AgentAdapter {
  id: "claude-code" | "codex" | string;
  detect(): Promise<{ installed: boolean; version?: string; supported: boolean }>;
  listModels(): Promise<Model[]>;          // from the agent itself, not a remote manifest
  startSession(opts: { cwd: string; model: string; permissionMode: PermissionMode;
                       resumeId?: string; systemPreamble?: string; mcpServers?: McpConfig[] }): Session;
}
interface Session {
  send(input: UserInput): AsyncIterable<AgentEvent>;   // normalized event stream
  respondToApproval(id: string, decision: Decision): void;
  setModel?(model: string): Promise<void>;             // if the agent supports in-session switching
  interrupt(): Promise<void>;
  close(): Promise<void>;
}
```

`AgentEvent` is Hearth's own normalized union: `text.delta`, `reasoning.delta`, `tool.start`, `tool.output`, `tool.end`, `file.edit`, `command.run`, `approval.request`, `usage`, `turn.end`, `error`. The raw agent event is kept alongside for debugging (stored locally only).

- **Codex adapter**: spawns `codex app-server`, speaks JSON-RPC over stdio, and maps thread, turn, and item notifications to `AgentEvent`.
- **Claude Code adapter**: uses the Claude Agent SDK `query()` with `canUseTool` for approvals and `resume` for continuity, running the user's installed `claude` executable. It lives in a small bundled Node sidecar (see §5). Fallback if the SDK route is blocked: `claude -p --input-format stream-json --output-format stream-json` directly from Rust.

### 4.4 Model switching and hand-off

- **Same agent, different model**: call `setModel` if supported. Otherwise resume the same agent session with the new model. Context is preserved natively.
- **Different agent** (e.g. Claude to Codex): Hearth opens a new session for the target agent in the **same working directory/worktree**, so files carry over. It seeds the session with a **hand-off preamble** built locally from the Hearth transcript:
  - the original goal, and the user's latest instruction
  - a condensed transcript (user messages in full, agent messages summarized to key decisions, file paths touched, and commands run with exit codes)
  - the current `git status` and `git diff --stat`
  - The preamble is shown to the user and editable before it's sent.
- **Agent-initiated hand-off** ("let Claude hand a task to Codex"): Hearth starts a **local, thread-scoped MCP server** (stdio, no network listener) and gives it to each agent session. It exposes:
  - `hearth.handoff(target_agent, task, context_notes)`: asks the user to confirm, then runs the task as a **sub-thread** with the target agent in the same worktree and returns its final message to the caller.
  - `hearth.thread_info()`: returns the current branch, worktree path, and changed files.
  - No other tools in v1. Every call is logged in the transcript.

### 4.5 Git

- **New thread options**: "Work in repo" (current checkout) or "Work in new worktree" (default). Worktrees are created at `<repo>/../.hearth-worktrees/<repo>/<slug>` (configurable) on branch `hearth/<slug>` from a chosen base (default: current branch).
- **Diff panel**: the whole thread's diff against base, plus **per-turn diffs** (Hearth snapshots `git write-tree` of the worktree index at turn boundaries using a temporary index, so the real index is untouched). It has a side-by-side and unified toggle, file tree, and syntax highlighting.
- **One-button ship**: **Commit** (message drafted by the thread's current agent, editable), then **Push** (`git push -u`), then **Open PR** (`gh pr create` with an agent-drafted, editable title and body; opens in the browser afterwards). Each step can also run on its own. Nothing is pushed or opened without an explicit click.
- **Cleanup**: archiving a thread offers "remove worktree" (refuses if there are uncommitted changes unless confirmed) and "delete branch".
- Uses the user's own `git` and `gh`. Hearth stores **no tokens**.

---

## 5. Architecture

```
┌──────────────────────── Hearth.app (Tauri v2) ───────────────────────┐
│  UI (WKWebView): React + TypeScript                                  │
│    • virtualized transcript, diff viewer, command palette            │
│    • CSP: no external origins; IPC only                              │
│                   ▲  Tauri IPC (typed commands + event channel)      │
│                   ▼                                                  │
│  Core (Rust)                                                         │
│    • thread/turn store (SQLite, WAL)    • git & worktree service     │
│    • process supervisor (agents, sidecar, gh)                        │
│    • adapter host: Codex (JSON-RPC stdio, in Rust)                   │
│    • local MCP server (stdio, per session)                           │
│    • file-access guard, egress policy                                │
│                   ▲ stdio (newline-delimited JSON)                   │
│                   ▼                                                  │
│  Claude sidecar (Node, bundled): Claude Agent SDK → user's `claude`  │
└──────────────────────────────────────────────────────────────────────┘
          spawns ▼                 spawns ▼              spawns ▼
        `codex app-server`         `claude`             `git`, `gh`
   (talks to OpenAI with        (talks to Anthropic    (talks to your
    the user's own login)        with the user's own    git remote / GitHub)
                                 login)
```

**Key decisions**

| Decision | Choice | Why |
|---|---|---|
| Shell | **Tauri v2** (not Electron) | Smaller binary and memory, native WKWebView, strict capability system, and a Rust core with no general-purpose network stack. |
| UI | React 19 + TypeScript, Vite, TanStack Virtual, Zustand, CodeMirror 6 for diffs | Mature tools; virtualization is mandatory for long threads. |
| Core | Rust (tokio, rusqlite, serde) | Predictable latency, no GC pauses, and easy process supervision. |
| Storage | SQLite at `~/Library/Application Support/Hearth/hearth.db` | Local, inspectable, exportable. |
| Claude integration | Agent SDK in a Node sidecar (spike in M0 against the direct CLI `stream-json` route; pick one) | The SDK is the supported route for approvals and resume. The sidecar only talks stdio to the core and has no network access of its own beyond what the `claude` child process does. |
| Packages | pnpm workspace + Cargo workspace | — |

**Repo layout (proposed)**

```
apps/desktop/          Tauri app (src-tauri/ Rust core, src/ React UI)
crates/core/           store, git, supervisor, egress & fs guards
crates/adapter-codex/  app-server JSON-RPC client
crates/mcp-local/      thread-scoped stdio MCP server
sidecars/claude/       Node Agent SDK bridge
packages/protocol/     shared AgentEvent / IPC types (TS + generated Rust)
fixtures/              recorded agent sessions for replay tests
docs/                  ADRs, CONTEXT.md (PRIVACY.md lives at the repo root)
```

---

## 6. Privacy & security (non-negotiable)

### 6.1 Guarantees (published in `PRIVACY.md`, each one tested)

1. **No telemetry, analytics, crash reporting, or "usage pings"**: no code for them exists in the codebase.
2. **Hearth itself makes no network requests** in v1. Network traffic comes only from the tools you already trust and run: `claude`, `codex`, `git`, `gh`, and links you click open in your browser.
3. **No accounts, no identifiers.** No install ID, device ID, or fingerprint is generated.
4. **Never reads agent credentials or config** (`~/.claude*`, `~/.codex/*`, keychain items, `gh` tokens).
5. **All data is local** and deletable: one folder, plus "Delete all Hearth data" in Settings.
6. **Diagnostics are manual.** "Export diagnostics" writes a local, redacted file and shows its contents before you save it. Hearth never uploads it.

### 6.2 Enforcement in code

- **Tauri capabilities**: the webview gets only Hearth's own IPC commands. No `http`, `shell`, or `fs` plugins are exposed to JS.
- **CSP**: `default-src 'self'; connect-src ipc: http://ipc.localhost; img-src 'self' data:` with no remote origins.
- **Rust core has no HTTP client crate.** `cargo-deny` bans `reqwest`, `hyper` client, `ureq`, `surf`, `isahc`, and similar in core crates.
- **File-access guard**: all core file reads go through one module with an allowlist (added project dirs, their worktrees, and the Hearth data dir). A denylist (`~/.claude*`, `~/.codex`, `~/.config/gh`, `~/.ssh`, `~/Library/Keychains`) panics in debug builds and is covered by tests.
- **Sidecar**: launched with a minimal env and `NODE_OPTIONS` cleared. Its dependency tree is locked and audited.
- **Agent processes** inherit the user's env (they need it to authenticate). Hearth never logs env values.

### 6.3 Enforcement in CI

- **Dependency ban list** (JS and Rust): `posthog*`, `@sentry/*`, `sentry`, `segment`, `@segment/*`, `mixpanel*`, `amplitude*`, `@amplitude/*`, `@axiomhq/*`, `@datadog/*`, `dd-trace`, `@opentelemetry/exporter-*`, `logrocket`, `fullstory`, `bugsnag`, `rollbar`, `@vercel/analytics`. The build fails on any match, including transitive ones.
- **Source grep gate**: fails on hard-coded `http(s)://` hosts in core/UI code outside an allowlist file (docs links only).
- **Egress test**: an end-to-end run of the app with stub agents under a network-denying sandbox profile (`sandbox-exec`) / packet capture asserts **zero outbound connections from Hearth's own processes**.
- **Privacy tests are protected**: changes to `PRIVACY.md` or `tests/privacy/**` require CODEOWNERS review, and the PR template asks "Does this change send any data off the machine?"

### 6.4 Supply chain & updates

- Builds are made locally (or in CI as an unpublished artifact) for the author's own Macs. No public releases, no download page.
- **Updates are manual**: pull and rebuild. Hearth has no updater and makes no update check.
- Lockfiles committed; Dependabot or Renovate with manual merge; `cargo audit` and `pnpm audit` in CI.
- No remote model manifests: model lists come from the installed agents themselves.

---

## 7. Performance (treated as a feature)

### 7.1 Budgets (M1-class Mac, release build)

| Metric | Budget |
|---|---|
| Cold start to interactive window | < 600 ms |
| Open a thread with 2,000 turns | < 150 ms to first paint |
| Keystroke to character on screen (composer) | < 16 ms p99 |
| Streaming render at 200 tokens/s | 0 dropped frames over 60 s (60 Hz) |
| Scroll a 10k-item transcript | ≥ 58 fps p95 |
| Diff panel, 5,000-line change | < 200 ms to render |
| Idle memory (5 open threads) | < 200 MB total app (excluding agent processes) |
| Event latency, agent stdout to pixels | < 30 ms p95 |

### 7.2 How we hit them

- Batch streaming deltas per animation frame (core coalesces and UI applies once per `requestAnimationFrame`).
- Virtualized transcript; markdown and highlighting run incrementally on the tail block only. Older blocks are memoized and frozen.
- Heavy work (diffing, highlighting big files, markdown for long blocks) runs in Rust or a Web Worker, never on the UI thread.
- SQLite writes are batched off the UI path; reads use prepared statements and indexes on `(thread_id, seq)`.

### 7.3 How we measure (locally only)

- A dev-only **perf HUD** (frame times, long tasks, IPC latency) that is compiled out of release builds.
- A **benchmark suite** that replays recorded agent sessions (`fixtures/`) at fixed speed and checks the budgets. It runs in CI on macOS runners and fails the build on regression beyond 10%.
- A dropped frame in the streaming benchmark is filed as a bug, same as a crash.

---

## 8. Data model (SQLite, sketch)

```
projects(id, path, name, created_at)
threads(id, project_id, title, cwd, worktree_path?, branch?, base_ref?, status, created_at, archived_at?)
agent_sessions(id, thread_id, agent_id, model, permission_mode, external_session_id?, started_at, ended_at?)
turns(id, thread_id, seq, agent_session_id, role, created_at, tree_snapshot?)
events(id, turn_id, seq, kind, payload_json, raw_json?)       -- normalized + raw
approvals(id, turn_id, request_json, decision, decided_at)
usage(turn_id, input_tokens, output_tokens, cache_read, cache_write)   -- local only
settings(key, value_json)
```

---

## 9. Milestones

Ordered by dependency, not dated: each one starts when the one before it is merged and its gates pass.

| # | Outcome |
|---|---|
| M0 | Repo scaffolding, CI (lint, test, privacy gates, egress test skeleton), Tauri shell, `PRIVACY.md`. **Spikes:** Claude Agent SDK sidecar vs direct CLI `stream-json`; Codex app-server handshake. An ADR for each. |
| M1 | One agent end to end: Codex thread with streaming, approvals, interrupt, persistence, transcript virtualization. Replay fixtures. |
| M2 | Claude Code adapter at parity. Model picker; same-agent model switch. |
| M3 | Worktrees per thread, diff panel (thread and per-turn), commit, push, PR via `gh`. |
| M4 | Cross-agent hand-off (user-initiated) and the local MCP `hearth.handoff` tool with sub-threads. |
| M5 | Performance pass against all budgets, local usage view, export diagnostics, signed and notarized build for the author's own Macs. |

**v1 is done when:** every §3 item works on a clean Mac with only `claude`, `codex`, `git`, and `gh` installed and signed in; every §6 CI gate is green; and every §7 budget passes in CI.

---

## 10. Open questions

1. **Name check**: "Hearth" may collide with other dev tools; it matters less for a private project, but confirm before any wider use.
2. Worktree location default: sibling folder (proposed) vs. a hidden dir next to `.git/` vs. the app data dir.
3. Do we want an **"egress ledger"** panel in v1.1 that shows, per agent process, which hosts it connected to (via `nettop`/`lsof`)? This makes the "your agents talk to their vendor, Hearth talks to no one" promise visible.
