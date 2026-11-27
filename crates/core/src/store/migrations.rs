//! Schema migrations, applied in order and tracked by `PRAGMA user_version`.

pub(super) const MIGRATIONS: &[&str] = &[
    // 1: initial schema (docs/spec.md section 8). Ids are TEXT.
    "
    CREATE TABLE projects (
        id TEXT PRIMARY KEY,
        path TEXT NOT NULL UNIQUE,
        name TEXT NOT NULL,
        created_at INTEGER NOT NULL
    );
    CREATE TABLE threads (
        id TEXT PRIMARY KEY,
        project_id TEXT NOT NULL REFERENCES projects(id),
        title TEXT NOT NULL,
        cwd TEXT NOT NULL,
        worktree_path TEXT,
        branch TEXT,
        base_ref TEXT,
        status TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        archived_at INTEGER
    );
    CREATE INDEX threads_by_project ON threads(project_id);
    CREATE TABLE agent_sessions (
        id TEXT PRIMARY KEY,
        thread_id TEXT NOT NULL REFERENCES threads(id),
        agent_id TEXT NOT NULL,
        model TEXT NOT NULL,
        permission_mode TEXT NOT NULL,
        external_session_id TEXT,
        started_at INTEGER NOT NULL,
        ended_at INTEGER
    );
    CREATE TABLE turns (
        id TEXT PRIMARY KEY,
        thread_id TEXT NOT NULL REFERENCES threads(id),
        seq INTEGER NOT NULL,
        agent_session_id TEXT NOT NULL REFERENCES agent_sessions(id),
        role TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        tree_snapshot TEXT,
        UNIQUE (thread_id, seq)
    );
    CREATE TABLE events (
        id TEXT PRIMARY KEY,
        turn_id TEXT NOT NULL REFERENCES turns(id),
        seq INTEGER NOT NULL,
        kind TEXT NOT NULL,
        payload_json TEXT NOT NULL,
        raw_json TEXT,
        UNIQUE (turn_id, seq)
    );
    CREATE TABLE approvals (
        id TEXT PRIMARY KEY,
        turn_id TEXT NOT NULL REFERENCES turns(id),
        request_json TEXT NOT NULL,
        decision TEXT NOT NULL,
        decided_at INTEGER NOT NULL
    );
    CREATE TABLE usage (
        turn_id TEXT PRIMARY KEY REFERENCES turns(id),
        input_tokens INTEGER NOT NULL,
        output_tokens INTEGER NOT NULL,
        cache_read INTEGER,
        cache_write INTEGER
    );
    CREATE TABLE settings (
        key TEXT PRIMARY KEY,
        value_json TEXT NOT NULL
    );
    ",
];
