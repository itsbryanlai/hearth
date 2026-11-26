# 0001. Claude Code integration: direct CLI stream-json, no Node sidecar

Status: accepted (maintainer ratified route B on 2026-11-27). Date: 2026-11-27.

## Context

Hearth must drive the user's installed `claude` for streaming text, tool approvals, interrupt, model switch and session resume (`docs/spec.md` §4.3). The spec named two routes and asked for a spike in M0 to pick one: the Claude Agent SDK in a bundled Node sidecar (route A), or the `claude` CLI in `stream-json` mode spawned straight from the Rust core (route B). Hearth's own processes must make no network requests and must not read the agent's credential or config files (§6).

The spike used `claude` 2.1.328, `@anthropic-ai/claude-agent-sdk` 0.3.332 (exact pin), Node 22, and a throwaway git repo. Real turns were limited to 12 on Haiku plus one Sonnet turn for the model switch (the Sonnet turn is one of the 12). All 12 were used: route A 6 (allowed edit, denied edit, resume, interrupt, and two for the model switch), route B 6 (simple, allowed edit, denied edit, interrupt, a repeat interrupt after a harness bug cut the first one short, resume).

## Options

**A. Agent SDK in a Node sidecar.** `query()` with `canUseTool`, `resume`, `interrupt()`, `setModel()`, `includePartialMessages`, and `pathToClaudeCodeExecutable` pointing at the user's `claude`. The sidecar speaks stdio to the Rust core.

**B. Direct CLI from Rust.** The core spawns `claude -p --input-format stream-json --output-format stream-json --verbose --include-partial-messages --permission-prompt-tool stdio`, writes user messages and control responses to stdin, and parses stdout lines.

Route B needs no MCP server for approvals. `--permission-prompt-tool stdio` makes the CLI emit `control_request` lines with `subtype: "can_use_tool"` on stdout and read `control_response` lines from stdin. This is the exact argument the SDK passes to its child (seen in the SDK's spawn arguments), so route A is a wrapper over the same wire protocol. `claude --help` documents `--permission-prompt-tool` and the stream-json formats but not the `stdio` value or the control messages; their shapes are visible in the SDK's `sdk.d.ts` and in the saved fixtures.

## Evidence

Capability checks (real turns; "yes" means observed, not inferred):

| Check | A: SDK sidecar | B: direct CLI |
|---|---|---|
| (a) approval reaches the host; allow and deny both work | Yes. `canUseTool` called once for `Edit`; allow edited `notes.txt`; deny left it unchanged and the model reported the denial. | Yes. `control_request` `can_use_tool` for `Edit`; we replied `{"behavior":"allow","updatedInput":...}` (file edited) or `{"behavior":"deny","message":...}` (file unchanged, model reported the denial). |
| (b) `resume` continues a session | Yes. New process with `resume: <id>`; the model recalled the first message. | Yes. `--resume <id>`; the model recalled the first message. |
| (c) interrupt stops a running turn | Yes. `interrupt()` returned in 9 ms and the turn ended with `result` `error_during_execution` 7.6 ms later. | Yes. `control_request` `interrupt` got a success `control_response`, then `result` `error_during_execution`. Output stopped at the interrupt (3,000-number prompt cut short). |
| (d) model switch mid-session | Yes. `setModel('sonnet')` between turns; the next assistant message reported `claude-sonnet-5-5` (the first reported `claude-haiku-5-5`). | Accepted, not switched to a different model. `set_model` control request returned success with `{"model":"claude-haiku-5-5"}`. It is the same message `setModel()` sends (SDK source), so A's Sonnet result covers it; B was not run on Sonnet to stay inside the one-Sonnet-turn limit. |
| (e) incremental streaming | Yes. `stream_event` `text_delta` messages with `includePartialMessages`. | Yes. Same events with `--include-partial-messages`. |

Measurements (this machine, Apple silicon, 5 to 6 runs each, short prompts; Haiku latency dominates anything below):

| Measure | A: SDK sidecar | B: direct CLI |
|---|---|---|
| Cold start to first child stdout line | 655 to 1,184 ms (median about 705 ms) | 735 to 962 ms |
| Cold start to first text delta | 1.8 s (no tools) to 7.5 s (tool turn); model time | 1.9 s (no tools) to 6.4 s (tool turn); model time |
| Child stdout line to parsed event in the host process | p50 0.1 to 0.2 ms, max 2.5 ms (58 to 101 events per run, SDK iterator, child stdout tapped through `spawnClaudeCodeProcess`) | p50 0.005 to 0.018 ms, max 0.14 ms (Rust `serde_json`, read thread) |
| Host process RSS | Node sidecar 49 to 66 MB | Rust spike binary 1.5 to 2.0 MB |
| Installed size | `node_modules` 432 MB with the platform binary the SDK ships as an optional dependency; about 63 MB without it (we pass `pathToClaudeCodeExecutable`, so it is not needed). Node itself must also be bundled and is not counted. | spike binary 720 KB (release) |
| `claude` child RSS | 246 to 287 MB at exit (measured in route B; the same child runs in A) | same |

The first-stdout time is the `claude` process startup and is the same for both routes. The extra hop in A costs under 3 ms per event and a 50 to 66 MB process; neither is visible next to model latency, so the choice does not rest on speed.

Privacy audit of the installed SDK (`sdk.mjs`, a 1.4 MB minified bundle in 178 lines; locations are `line:column`):

- No `http`, `https`, `tls` or `dgram` import. `net` is imported only for IP-address parsing (`sdk.mjs:146:61130`).
- `fetch(` appears twice, both in the remote-session helper that POSTs and DELETEs `${serverUrl}/sessions` against a URL the caller supplies (`sdk.mjs:143:2675`, `143:3345`). A `new WebSocket(` in the same feature (`sdk.mjs:141:2917`). None run in the `query()` path we would use.
- Hard-coded hosts in the bundle are the CLI's OAuth and update endpoints (`platform.claude.com`, `api.anthropic.com/api/oauth/...`, `sdk.mjs:52:3989` to `52:4120`), an MCP proxy (`mcp-proxy.anthropic.com`, `52:4769`), the model catalog and release downloads (`downloads.claude.ai`, `148:80955`, `148:128916`). The bundle includes the CLI's settings schema, so these are constants and documentation strings, not calls the sidecar makes. The real traffic comes from the `claude` child, in both routes.
- Telemetry switches read by the bundle: `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC`, `DISABLE_TELEMETRY` and `DO_NOT_TRACK` (`sdk.mjs:40:4317`), plus the opt-in `CLAUDE_CODE_ENABLE_TELEMETRY` and the `OTEL_*` family (104 mentions). Feature flags are described as "server-controlled via GrowthBook" in the settings text (`sdk.mjs:148:132546`).
- Dependency scan against the `AGENTS.md` ban list: nothing banned. `@opentelemetry/api` is installed; no `@opentelemetry/exporter-*`, Sentry, PostHog, Segment or Datadog package.
- In both routes the child's `system` `init` event reports `analytics_disabled: false` and `product_feedback_disabled: false` when launched with no extra environment. Whether the three env vars above flip it was not tested (a test needs a model turn, and the budget was spent). M2 must test it before relying on them. (Open: the follow-up turn that tests `DISABLE_TELEMETRY=1`, `DO_NOT_TRACK=1` and `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` on the child is not yet recorded here, and `fixtures/claude/telemetry-disabled.jsonl` does not exist yet.)

Fixtures (`fixtures/claude/*.jsonl`) are raw `claude` stdout, which is identical for both routes: `simple`, `approval-allowed`, `approval-denied`, `interrupt`, `resume`. The host's own stdin replies are not in stdout; their shapes are in the table above. Scrubbed by a script that is not committed: `/Users/<name>` to `$HOME`; the OS username to `USER` (also inside the munged project directory name); `/private/tmp/claude-<uid>` to `claude-UID`; the messaging socket's process id to `PID`; and any value of these field names to `REDACTED`: `email`, `emailAddress`, `accountUuid`, `account_uuid`, `organizationUuid`, `organization_uuid`, `organizationName`, `organization_name`, `displayName`, `userID`, `user_id`, `orgId`, `org_id` (none were present in these streams). Input JSON fragments that split a path across `partial_json` deltas were rejoined, scrubbed and re-split. Left as is: random session and message ids, built-in plugin and skill names, and the `apiKeySource` label. Any string that contains a path into `~/.claude*` (the `init` event's `memory_paths`) is replaced with `AGENT_CONFIG_PATH_REDACTED`; nothing under `~/.claude*` was opened.

## Finding: ambient `ANTHROPIC_API_KEY`

Every `init` event in the fixtures reports `apiKeySource: "ANTHROPIC_API_KEY"`, although the spike set no key variable. The spawned `claude` inherited one from the parent environment. When that variable is present the CLI authenticates with the key and bills the API, not the user's own `claude` login, so a Hearth that blindly inherits the environment can spend a key the user never meant it to use. This also conflicts with the spec's promise that Hearth only spawns the user's signed-in CLIs.

Proposal for the maintainer (not decided): in M2, spawn the child with an explicit allowlist of variables, not the full inherited environment.

- Pass: `PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, `TMPDIR`, `LANG`, `LC_*`, `TERM`, `SSH_AUTH_SOCK` only if tools need it, and proxy and certificate variables (`HTTPS_PROXY`, `NO_PROXY`, `SSL_CERT_FILE`, `NODE_EXTRA_CA_CERTS`) so corporate setups keep working.
- Drop by default: `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_BASE_URL`, `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_VERTEX` and other provider or credential variables, plus `NODE_OPTIONS`.
- Settings: a switch "Use ANTHROPIC_API_KEY from my environment" (off by default). When on, Hearth passes only that one variable and shows "billed to API key" in the session header, taken from `apiKeySource` in `init`.
- Telemetry variables: set `DISABLE_TELEMETRY=1`, `DO_NOT_TRACK=1` and `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` on every spawn, once the pending test above shows they take effect.
- Hearth never logs env values; it may log variable names that were dropped.

The spec line "Agent processes inherit the user's env" (§6.2) would change if this is accepted.

## Decision

**Route B: spawn `claude` directly from the Rust core and speak the stream-json control protocol. No Node sidecar.**

Reasons: every capability Hearth needs (a to e) works on the wire without the SDK; the SDK is a thin wrapper that sends the same control requests; and dropping it removes a bundled Node runtime, a 63 MB dependency tree, a 50 to 66 MB process, one more process boundary to supervise, and 1.4 MB of minified third-party code we would have to audit on every bump for the §6 rules. A single Rust core owning the child also keeps the file-access guard and egress rules in one place.

Revisit if any of these happen:

- A `claude` release changes or removes `--permission-prompt-tool stdio`, the `can_use_tool`, `interrupt` or `set_model` control messages, or the event shapes in the fixtures, and the SDK is the only documented route to the new behavior.
- Anthropic publishes the control protocol as supported only through the SDK, or the CLI route loses a capability the SDK keeps (for example SDK-hosted MCP tools or hooks callbacks that Hearth later needs).
- Maintaining the protocol layer in Rust costs more than M2 estimates (more than a small module plus fixtures).

## Consequences

What M2 builds:

- A `claude` adapter module in the Rust core: process supervision, a line reader, and a typed decoder for the events in `fixtures/claude/`, mapped to `AgentEvent`. Unknown event types and fields are kept in the raw event and ignored, not errors.
- Approval handling: a `can_use_tool` request becomes `approval.request`; `respondToApproval` writes the `control_response` with `allow` (plus `updatedInput`) or `deny` (plus `message`). `request_id` links the two.
- `interrupt()` and `setModel()` as control requests; `resume` as `--resume <id>`. Interrupt ends the turn with `error_during_execution`; the adapter maps that to `turn.end` with an interrupted flag, not `error`.
- Replay tests built from the fixtures; a version gate in `detect()` against the tested `claude` versions (this spike: 2.1.328).
- The spawn must set the environment deliberately. The child inherits the parent's environment; Hearth decides which variables pass through and whether to set the telemetry-off variables, after M2 checks they take effect.
- The spec's sidecar diagram and repo layout lines are stale (see the report); remove `sidecars/claude/` when the adapter lands.

Costs accepted: the protocol is observable but not documented in `--help`, so a CLI update can break it; the version gate and fixtures are the guard. A real-agent smoke test per supported version stays manual and maintainer-approved.
