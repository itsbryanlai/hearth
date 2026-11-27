# Hearth

One local home for all your coding agents.

Hearth is a macOS desktop app that drives the coding agents you already have installed and signed in to (Claude Code, Qwen Code) from one window, with git worktrees, diff review, and pull requests built in.

- **Your subscriptions and keys.** Hearth starts the agent CLIs on your machine. It never sells, proxies, or sees tokens, and never reads agent login files.
- **Every agent in one place.** Switch model mid-thread. Hand a task from one agent to another.
- **Git built in.** One worktree per thread, then review the diff and commit, push, and open a PR.
- **Fast.** Performance budgets are measured and enforced in CI.
- **Private by construction.** No telemetry, no analytics, no accounts, no servers. Nothing leaves your machine except what the agents, `git`, and `gh` you already use send themselves.

## Status

Pre-alpha. See [docs/spec.md](docs/spec.md) for the v1 spec.

## Not for public use

Hearth is a personal project for the author's own use. It is not distributed, supported, or licensed for use by anyone else. The source is visible, but no license is granted: see [LICENSE](LICENSE).
