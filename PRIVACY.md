# Privacy

Hearth runs on your machine and keeps everything there. These are the six promises it makes, in plain words, and the check that fails if one is broken.

The dependency ban lists live in one place: `scripts/privacy-gates.sh`. Changes to this file, the gate script, its allowlist and `tests/privacy/**` need review from the code owner (`.github/CODEOWNERS`), and every pull request answers "Does this change send any data off the machine?"

| # | Promise | Enforced by |
|---|---------|-------------|
| 1 | **No telemetry, analytics, crash reporting or usage pings.** No code for them exists. | `scripts/privacy-gates.sh` fails the build if any analytics or crash-reporting package (for example `posthog*`, `@sentry/*`, `mixpanel*`), direct or transitive, appears in `pnpm-lock.yaml` or a Rust crate's dependency tree. Tested by `scripts/tests/test-privacy-gates.sh`. |
| 2 | **Hearth itself makes no network requests.** Traffic comes only from tools you already run and trust: `claude`, `codex`, `git`, `gh`, and links you open in your browser. | Rust core crates may not depend on an HTTP client (`reqwest`, `hyper`, `ureq`, `surf`, `isahc`): `scripts/privacy-gates.sh`. Hard-coded `http(s)://` hosts in app code fail the same script unless listed in `scripts/privacy-allowlist.txt`. The closed webview CSP is reviewed under the code-owner rule. The egress test (the app run under a network-denying sandbox, asserting zero outbound connections) is enforced from M0 (lands with the egress-test change). |
| 3 | **No accounts, no identifiers.** No install ID, device ID or fingerprint is generated or stored. | Review rule: the pull request question above, plus code-owner review of `tests/privacy/**`. Dependency gate blocks analytics SDKs that create identifiers. |
| 4 | **Never reads agent credentials or config** (`~/.claude*`, `~/.codex/`, keychain items, `gh` tokens). | The file-access guard with a denylist and its tests: enforced from M1. Until then, the code-owner review rule applies. |
| 5 | **All data is local** and deletable: one folder, plus "Delete all Hearth data" in Settings. | Egress test (promise 2) shows nothing leaves the machine; the Settings delete action is covered by its own test when it lands (M1). |
| 6 | **Diagnostics are manual.** "Export diagnostics" writes a local, redacted file and shows you its contents first. Hearth never uploads it. | No upload path can exist: promise 2's gates and egress test. The export-preview test lands with the feature (M1). |

"Enforced from" means the check is not in the tree yet; until it lands, the promise is held by the review rules above.
