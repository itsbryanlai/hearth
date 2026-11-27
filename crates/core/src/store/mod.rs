//! Local SQLite store (docs/spec.md section 8). Synchronous; the caller
//! decides which thread owns the [`Store`]. The database path is passed in:
//! no global state, no home-directory lookup, no other file access.

mod migrations;

use hearth_protocol::{
    AgentEvent, AgentId, AgentSessionId, ApprovalDecision, ApprovalId, ApprovalRequest, EventId,
    PermissionMode, ProjectId, ThreadId, TokenUsage, TurnId, UserInput,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::fmt;
use std::path::Path;

#[derive(Debug)]
pub enum StoreError {
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    /// The database was written by a newer Hearth.
    SchemaTooNew(i64),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "database error: {e}"),
            Self::Json(e) => write!(f, "stored json is invalid: {e}"),
            Self::SchemaTooNew(v) => {
                write!(f, "database schema version {v} is newer than this build")
            }
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ThreadStatus {
    Idle,
    Running,
    AwaitingApproval,
    Error,
    Archived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TurnRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: ProjectId,
    pub path: String,
    pub name: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Default)]
pub struct NewThread<'a> {
    pub title: &'a str,
    pub cwd: &'a str,
    pub worktree_path: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub base_ref: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thread {
    pub id: ThreadId,
    pub project_id: ProjectId,
    pub title: String,
    pub cwd: String,
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
    pub base_ref: Option<String>,
    pub status: ThreadStatus,
    pub created_at: i64,
    pub archived_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub id: TurnId,
    pub thread_id: ThreadId,
    pub seq: i64,
    pub agent_session_id: AgentSessionId,
    pub role: TurnRole,
    pub created_at: i64,
    pub tree_snapshot: Option<String>,
    /// What the user sent, on a user turn.
    pub input: Option<UserInput>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSession {
    pub id: AgentSessionId,
    pub agent: AgentId,
    pub model: String,
    pub external_session_id: Option<String>,
}

/// An event to append: the normalized event plus the agent's raw payload.
#[derive(Debug, Clone, PartialEq)]
pub struct NewEvent {
    pub event: AgentEvent,
    pub raw: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredEvent {
    pub id: EventId,
    pub seq: i64,
    pub event: AgentEvent,
    pub raw: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadedTurn {
    pub turn: Turn,
    pub agent: AgentId,
    pub model: String,
    pub events: Vec<StoredEvent>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadedThread {
    pub thread: Thread,
    pub turns: Vec<LoadedTurn>,
}

pub struct Store {
    conn: Connection,
}

const NOW_MS: &str = "CAST(strftime('%s','now') AS INTEGER) * 1000";

fn to_text<T: Serialize>(v: &T) -> Result<String> {
    match serde_json::to_value(v)? {
        serde_json::Value::String(s) => Ok(s),
        other => Ok(other.to_string()),
    }
}

fn from_text<T: DeserializeOwned>(s: &str) -> Result<T> {
    Ok(serde_json::from_value(serde_json::Value::String(
        s.to_owned(),
    ))?)
}

fn json_err(e: StoreError) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(e))
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        // journal_mode returns a row; in-memory databases answer "memory".
        conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get::<_, String>(0))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&mut self) -> Result<()> {
        let current: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let target = migrations::MIGRATIONS.len() as i64;
        if current > target {
            return Err(StoreError::SchemaTooNew(current));
        }
        for (i, sql) in migrations::MIGRATIONS
            .iter()
            .enumerate()
            .skip(current as usize)
        {
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", i as i64 + 1)?;
            tx.commit()?;
        }
        Ok(())
    }

    pub fn add_project(&mut self, path: &Path, name: &str) -> Result<Project> {
        let id = ProjectId::new();
        let path = path.to_string_lossy().into_owned();
        self.conn.execute(
            &format!(
                "INSERT INTO projects (id, path, name, created_at) VALUES (?1, ?2, ?3, {NOW_MS})"
            ),
            params![id.0, path, name],
        )?;
        let created_at = self.conn.query_row(
            "SELECT created_at FROM projects WHERE id = ?1",
            [&id.0],
            |r| r.get(0),
        )?;
        Ok(Project {
            id,
            path,
            name: name.to_owned(),
            created_at,
        })
    }

    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, path, name, created_at FROM projects ORDER BY created_at, id")?;
        let rows = stmt.query_map([], |r| {
            Ok(Project {
                id: ProjectId(r.get(0)?),
                path: r.get(1)?,
                name: r.get(2)?,
                created_at: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn create_thread(&mut self, project: &ProjectId, new: NewThread<'_>) -> Result<Thread> {
        let id = ThreadId::new();
        self.conn.execute(
            &format!(
                "INSERT INTO threads (id, project_id, title, cwd, worktree_path, branch, base_ref, status, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, {NOW_MS})"
            ),
            params![
                id.0,
                project.0,
                new.title,
                new.cwd,
                new.worktree_path,
                new.branch,
                new.base_ref,
                to_text(&ThreadStatus::Idle)?
            ],
        )?;
        self.get_thread(&id)
    }

    /// Creates the thread and its first session together, or neither.
    pub fn create_thread_with_session(
        &mut self,
        project: &ProjectId,
        new: NewThread<'_>,
        agent: &AgentId,
    ) -> Result<Thread> {
        let id = ThreadId::new();
        let session = AgentSessionId::new();
        let tx = self.conn.transaction()?;
        tx.execute(
            &format!(
                "INSERT INTO threads (id, project_id, title, cwd, worktree_path, branch, base_ref, status, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, {NOW_MS})"
            ),
            params![
                id.0,
                project.0,
                new.title,
                new.cwd,
                new.worktree_path,
                new.branch,
                new.base_ref,
                to_text(&ThreadStatus::Idle)?
            ],
        )?;
        tx.execute(
            &format!(
                "INSERT INTO agent_sessions (id, thread_id, agent_id, model, permission_mode, external_session_id, started_at)
                 VALUES (?1, ?2, ?3, '', ?4, NULL, {NOW_MS})"
            ),
            params![
                session.0,
                id.0,
                to_text(agent)?,
                to_text(&PermissionMode::Default)?
            ],
        )?;
        let thread = tx.query_row(THREAD_SELECT_BY_ID, [&id.0], thread_from_row)?;
        tx.commit()?;
        Ok(thread)
    }

    pub fn get_thread(&self, id: &ThreadId) -> Result<Thread> {
        Ok(self
            .conn
            .query_row(THREAD_SELECT_BY_ID, [&id.0], thread_from_row)?)
    }

    pub fn list_threads(&self, project: &ProjectId) -> Result<Vec<Thread>> {
        let mut stmt = self.conn.prepare(&format!(
            "{THREAD_SELECT} WHERE project_id = ?1 ORDER BY created_at, id"
        ))?;
        let rows = stmt.query_map([&project.0], thread_from_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Setting `Archived` also stamps `archived_at`; any other status clears it.
    pub fn update_thread_status(&mut self, id: &ThreadId, status: ThreadStatus) -> Result<()> {
        let archived = if status == ThreadStatus::Archived {
            format!("COALESCE(archived_at, {NOW_MS})")
        } else {
            "NULL".to_owned()
        };
        self.conn.execute(
            &format!("UPDATE threads SET status = ?1, archived_at = {archived} WHERE id = ?2"),
            params![to_text(&status)?, id.0],
        )?;
        Ok(())
    }

    pub fn start_agent_session(
        &mut self,
        thread: &ThreadId,
        agent: &AgentId,
        model: &str,
        permission_mode: PermissionMode,
        external_session_id: Option<&str>,
    ) -> Result<AgentSessionId> {
        let id = AgentSessionId::new();
        self.conn.execute(
            &format!(
                "INSERT INTO agent_sessions (id, thread_id, agent_id, model, permission_mode, external_session_id, started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, {NOW_MS})"
            ),
            params![id.0, thread.0, to_text(agent)?, model, to_text(&permission_mode)?, external_session_id],
        )?;
        Ok(id)
    }

    /// Starts the next turn of a thread (`seq` is one more than the last).
    pub fn start_turn(
        &mut self,
        thread: &ThreadId,
        session: &AgentSessionId,
        role: TurnRole,
        tree_snapshot: Option<&str>,
    ) -> Result<Turn> {
        let id = TurnId::new();
        let tx = self.conn.transaction()?;
        let seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM turns WHERE thread_id = ?1",
            [&thread.0],
            |r| r.get(0),
        )?;
        tx.execute(
            &format!(
                "INSERT INTO turns (id, thread_id, seq, agent_session_id, role, created_at, tree_snapshot)
                 VALUES (?1, ?2, ?3, ?4, ?5, {NOW_MS}, ?6)"
            ),
            params![id.0, thread.0, seq, session.0, to_text(&role)?, tree_snapshot],
        )?;
        let turn = tx.query_row(TURN_SELECT_BY_ID, [&id.0], turn_from_row)?;
        tx.commit()?;
        Ok(turn)
    }

    pub fn turn_tree_pair(
        &self,
        thread: &ThreadId,
        turn: &TurnId,
    ) -> Result<Option<(Option<String>, Option<String>)>> {
        Ok(self.conn.query_row(
            "SELECT (SELECT tree_snapshot FROM turns previous WHERE previous.thread_id = current.thread_id AND previous.seq = current.seq - 1 AND previous.role = 'user'), current.tree_snapshot FROM turns current WHERE current.thread_id = ?1 AND current.id = ?2 AND current.role = 'assistant'",
            params![thread.0, turn.0], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?)
    }

    pub fn set_turn_tree_snapshot(&mut self, turn: &TurnId, tree: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE turns SET tree_snapshot = ?1 WHERE id = ?2",
            params![tree, turn.0],
        )?;
        Ok(())
    }

    /// Appends a batch in one transaction; `seq` continues from the turn's last event.
    pub fn append_events(&mut self, turn: &TurnId, events: &[NewEvent]) -> Result<()> {
        let tx = self.conn.transaction()?;
        let mut seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM events WHERE turn_id = ?1",
            [&turn.0],
            |r| r.get(0),
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO events (id, turn_id, seq, kind, payload_json, raw_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for e in events {
                seq += 1;
                let payload = serde_json::to_value(&e.event)?;
                let kind = payload
                    .get("kind")
                    .and_then(|k| k.as_str())
                    .unwrap_or_default()
                    .to_owned();
                let raw = e.raw.as_ref().map(|r| r.to_string());
                stmt.execute(params![
                    EventId::new().0,
                    turn.0,
                    seq,
                    kind,
                    payload.to_string(),
                    raw
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The thread with its turns (by `seq`) and each turn's events (by `seq`).
    pub fn load_thread(&self, id: &ThreadId) -> Result<LoadedThread> {
        let thread = self.get_thread(id)?;
        let mut turn_stmt = self.conn.prepare(&format!(
            "SELECT turn.*, session.agent_id, session.model
             FROM ({TURN_SELECT} WHERE thread_id = ?1) turn
             JOIN agent_sessions session ON session.id = turn.agent_session_id
             ORDER BY turn.seq"
        ))?;
        let turns: Vec<_> = turn_stmt
            .query_map([&id.0], |row| {
                let agent: String = row.get(8)?;
                Ok((
                    turn_from_row(row)?,
                    from_text::<AgentId>(&agent).map_err(json_err)?,
                    row.get::<_, String>(9)?,
                ))
            })?
            .collect::<std::result::Result<_, _>>()?;
        let mut ev_stmt = self.conn.prepare(
            "SELECT id, seq, payload_json, raw_json FROM events WHERE turn_id = ?1 ORDER BY seq",
        )?;
        let mut loaded = Vec::with_capacity(turns.len());
        for (turn, agent, model) in turns {
            let events = ev_stmt
                .query_map([&turn.id.0], |r| {
                    let payload: String = r.get(2)?;
                    let raw: Option<String> = r.get(3)?;
                    Ok(StoredEvent {
                        id: EventId(r.get(0)?),
                        seq: r.get(1)?,
                        event: serde_json::from_str(&payload).map_err(|e| json_err(e.into()))?,
                        raw: raw
                            .map(|s| serde_json::from_str(&s))
                            .transpose()
                            .map_err(|e| json_err(e.into()))?,
                    })
                })?
                .collect::<std::result::Result<_, _>>()?;
            loaded.push(LoadedTurn {
                turn,
                agent,
                model,
                events,
            });
        }
        Ok(LoadedThread {
            thread,
            turns: loaded,
        })
    }

    /// Records what the user sent on a user turn.
    pub fn set_turn_input(&mut self, turn: &TurnId, input: &UserInput) -> Result<()> {
        self.conn.execute(
            "UPDATE turns SET input_json = ?1 WHERE id = ?2",
            params![serde_json::to_string(input)?, turn.0],
        )?;
        Ok(())
    }

    pub fn set_thread_title(&mut self, id: &ThreadId, title: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE threads SET title = ?1 WHERE id = ?2",
            params![title, id.0],
        )?;
        Ok(())
    }

    pub fn selected_model(&self, thread: &ThreadId) -> Result<Option<String>> {
        self.get_setting(&model_key(thread))?
            .map(serde_json::from_value)
            .transpose()
            .map_err(Into::into)
    }

    /// Commits the chosen selector and the new turn-attribution row together.
    pub fn select_model(
        &mut self,
        thread: &ThreadId,
        previous: &AgentSessionId,
        choice: &str,
        model: &str,
        external_id: Option<&str>,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        continue_session(&tx, previous, model, external_id)?;
        put_setting(
            &tx,
            &model_key(thread),
            &serde_json::Value::String(choice.into()),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Records the ready model on the first turn, or starts another attribution row.
    pub fn prepare_turn_session(
        &mut self,
        session: &AgentSession,
        model: &str,
        external_id: Option<&str>,
    ) -> Result<AgentSessionId> {
        if session.model == model && session.external_session_id.as_deref() == external_id {
            return Ok(session.id.clone());
        }
        if session.model.is_empty()
            || (session.model == model && session.external_session_id.is_none())
        {
            write_session(&self.conn, &session.id, model, external_id)?;
            Ok(session.id.clone())
        } else {
            continue_session(&self.conn, &session.id, model, external_id)
        }
    }

    /// Attributes a turn to the model reported after it was started.
    pub fn observe_turn_session(
        &mut self,
        turn: &Turn,
        model: &str,
        external_id: &str,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        let (previous, previous_model, previous_external, shared): (String, String, Option<String>, bool) = tx.query_row(
            "SELECT session.id, session.model, session.external_session_id, EXISTS(
                SELECT 1 FROM turns older
                WHERE older.agent_session_id = session.id AND older.role = 'assistant'
                  AND older.seq < current.seq
             ) FROM turns current JOIN agent_sessions session ON session.id = current.agent_session_id
             WHERE current.id = ?1",
            [&turn.id.0],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        let previous = AgentSessionId(previous);
        if shared && (previous_model != model || previous_external.as_deref() != Some(external_id))
        {
            let session = continue_session(&tx, &previous, model, Some(external_id))?;
            tx.execute(
                "UPDATE turns SET agent_session_id = ?1
                 WHERE thread_id = ?2 AND agent_session_id = ?3
                   AND (id = ?4 OR (seq = ?5 AND role = 'user'))",
                params![
                    session.0,
                    turn.thread_id.0,
                    previous.0,
                    turn.id.0,
                    turn.seq - 1
                ],
            )?;
        } else {
            write_session(&tx, &previous, model, Some(external_id))?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The thread's newest agent session.
    pub fn latest_agent_session(&self, thread: &ThreadId) -> Result<Option<AgentSession>> {
        let row: Option<(String, String, String, Option<String>)> = self
            .conn
            .query_row(
                "SELECT id, agent_id, model, external_session_id FROM agent_sessions
                 WHERE thread_id = ?1 ORDER BY started_at DESC, rowid DESC LIMIT 1",
                [&thread.0],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        row.map(|(id, agent, model, external_session_id)| {
            Ok(AgentSession {
                id: AgentSessionId(id),
                agent: from_text(&agent)?,
                model,
                external_session_id,
            })
        })
        .transpose()
    }

    /// Assistant turns with no `turn.end` event: turns that were cut off.
    pub fn unfinished_turns(&self) -> Result<Vec<(ThreadId, TurnId)>> {
        let mut stmt = self.conn.prepare(
            "SELECT thread_id, id FROM turns WHERE role = 'assistant'
             AND NOT EXISTS (SELECT 1 FROM events WHERE events.turn_id = turns.id AND kind = 'turn.end')
             ORDER BY thread_id, seq",
        )?;
        let rows = stmt.query_map([], |r| Ok((ThreadId(r.get(0)?), TurnId(r.get(1)?))))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// How many events the thread holds across all its turns.
    pub fn thread_event_count(&self, thread: &ThreadId) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM events WHERE turn_id IN (SELECT id FROM turns WHERE thread_id = ?1)",
            [&thread.0],
            |r| r.get(0),
        )?)
    }

    pub fn record_approval(
        &mut self,
        turn: &TurnId,
        request: &ApprovalRequest,
        decision: ApprovalDecision,
    ) -> Result<()> {
        self.conn.execute(
            &format!(
                "INSERT INTO approvals (id, turn_id, request_json, decision, decided_at) VALUES (?1, ?2, ?3, ?4, {NOW_MS})"
            ),
            params![request.id.0, turn.0, serde_json::to_string(request)?, to_text(&decision)?],
        )?;
        Ok(())
    }

    pub fn list_approvals(
        &self,
        turn: &TurnId,
    ) -> Result<Vec<(ApprovalRequest, ApprovalDecision)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT request_json, decision FROM approvals WHERE turn_id = ?1 ORDER BY decided_at, id")?;
        let rows: Vec<(String, String)> = stmt
            .query_map([&turn.0], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        rows.into_iter()
            .map(|(req, dec)| Ok((serde_json::from_str(&req)?, from_text(&dec)?)))
            .collect()
    }

    pub fn find_approval_decision(&self, id: &ApprovalId) -> Result<Option<ApprovalDecision>> {
        let text: Option<String> = self
            .conn
            .query_row(
                "SELECT decision FROM approvals WHERE id = ?1",
                [&id.0],
                |r| r.get(0),
            )
            .optional()?;
        text.map(|t| from_text(&t)).transpose()
    }

    /// Stores the latest usage for a turn (replaces an earlier value).
    pub fn record_usage(&mut self, turn: &TurnId, usage: &TokenUsage) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO usage (turn_id, input_tokens, output_tokens, cache_read, cache_write)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![turn.0, usage.input_tokens, usage.output_tokens, usage.cache_read_tokens, usage.cache_write_tokens],
        )?;
        Ok(())
    }

    pub fn get_usage(&self, turn: &TurnId) -> Result<Option<TokenUsage>> {
        Ok(self
            .conn
            .query_row(
                "SELECT input_tokens, output_tokens, cache_read, cache_write FROM usage WHERE turn_id = ?1",
                [&turn.0],
                |r| {
                    Ok(TokenUsage {
                        input_tokens: r.get(0)?,
                        output_tokens: r.get(1)?,
                        cache_read_tokens: r.get(2)?,
                        cache_write_tokens: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<serde_json::Value>> {
        let text: Option<String> = self
            .conn
            .query_row(
                "SELECT value_json FROM settings WHERE key = ?1",
                [key],
                |r| r.get(0),
            )
            .optional()?;
        Ok(text.map(|t| serde_json::from_str(&t)).transpose()?)
    }

    pub fn set_setting(&mut self, key: &str, value: &serde_json::Value) -> Result<()> {
        put_setting(&self.conn, key, value)
    }
}

fn model_key(thread: &ThreadId) -> String {
    format!("thread.{}.selectedModel", thread.0)
}

fn put_setting(conn: &Connection, key: &str, value: &serde_json::Value) -> Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value_json) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json",
        params![key, value.to_string()],
    )?;
    Ok(())
}

fn continue_session(
    conn: &Connection,
    previous: &AgentSessionId,
    model: &str,
    external_id: Option<&str>,
) -> Result<AgentSessionId> {
    let next = AgentSessionId::new();
    let affected = conn.execute(
        &format!(
            "INSERT INTO agent_sessions (id, thread_id, agent_id, model, permission_mode, external_session_id, started_at)
             SELECT ?1, thread_id, agent_id, ?2, permission_mode, ?3, {NOW_MS}
             FROM agent_sessions WHERE id = ?4"
        ),
        params![next.0, model, external_id, previous.0],
    )?;
    if affected == 0 {
        return Err(rusqlite::Error::QueryReturnedNoRows.into());
    }
    Ok(next)
}

fn write_session(
    conn: &Connection,
    id: &AgentSessionId,
    model: &str,
    external_id: Option<&str>,
) -> Result<()> {
    let affected = conn.execute(
        "UPDATE agent_sessions SET model = ?1, external_session_id = ?2 WHERE id = ?3",
        params![model, external_id, id.0],
    )?;
    if affected == 0 {
        return Err(rusqlite::Error::QueryReturnedNoRows.into());
    }
    Ok(())
}

const THREAD_SELECT: &str = "SELECT id, project_id, title, cwd, worktree_path, branch, base_ref, status, created_at, archived_at FROM threads";
const THREAD_SELECT_BY_ID: &str = "SELECT id, project_id, title, cwd, worktree_path, branch, base_ref, status, created_at, archived_at FROM threads WHERE id = ?1";
const TURN_SELECT: &str = "SELECT id, thread_id, seq, agent_session_id, role, created_at, tree_snapshot, input_json FROM turns";
const TURN_SELECT_BY_ID: &str = "SELECT id, thread_id, seq, agent_session_id, role, created_at, tree_snapshot, input_json FROM turns WHERE id = ?1";

fn thread_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Thread> {
    let status: String = r.get(7)?;
    Ok(Thread {
        id: ThreadId(r.get(0)?),
        project_id: ProjectId(r.get(1)?),
        title: r.get(2)?,
        cwd: r.get(3)?,
        worktree_path: r.get(4)?,
        branch: r.get(5)?,
        base_ref: r.get(6)?,
        status: from_text(&status).map_err(json_err)?,
        created_at: r.get(8)?,
        archived_at: r.get(9)?,
    })
}

fn turn_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Turn> {
    let role: String = r.get(4)?;
    let input: Option<String> = r.get(7)?;
    Ok(Turn {
        id: TurnId(r.get(0)?),
        thread_id: ThreadId(r.get(1)?),
        seq: r.get(2)?,
        agent_session_id: AgentSessionId(r.get(3)?),
        role: from_text(&role).map_err(json_err)?,
        created_at: r.get(5)?,
        tree_snapshot: r.get(6)?,
        input: input
            .map(|t| serde_json::from_str(&t))
            .transpose()
            .map_err(|e| json_err(e.into()))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearth_protocol::{ApprovalOption, ApprovalOptionKind, StopReason};

    struct Fixture {
        store: Store,
        thread: Thread,
        turn: Turn,
    }

    fn fixture() -> Fixture {
        let mut store = Store::open_in_memory().unwrap();
        let project = store.add_project(Path::new("/tmp/proj"), "proj").unwrap();
        let thread = store
            .create_thread(
                &project.id,
                NewThread {
                    title: "t",
                    cwd: "/tmp/proj",
                    ..Default::default()
                },
            )
            .unwrap();
        let session = store
            .start_agent_session(
                &thread.id,
                &AgentId::QwenCode,
                "qwen3",
                PermissionMode::Default,
                None,
            )
            .unwrap();
        let turn = store
            .start_turn(&thread.id, &session, TurnRole::User, None)
            .unwrap();
        Fixture {
            store,
            thread,
            turn,
        }
    }

    fn ev(e: AgentEvent) -> NewEvent {
        NewEvent {
            event: e,
            raw: None,
        }
    }

    // Fails if events lose order across batches, the raw payload is dropped,
    // or a reloaded event differs from the one stored.
    #[test]
    fn thread_round_trips_events_in_order() {
        let mut f = fixture();
        let first = vec![
            NewEvent {
                event: AgentEvent::TextDelta { text: "he".into() },
                raw: Some(serde_json::json!({"agent": "raw"})),
            },
            ev(AgentEvent::TextDelta { text: "llo".into() }),
        ];
        let second = vec![ev(AgentEvent::TurnEnd {
            stop_reason: StopReason::EndTurn,
        })];
        f.store.append_events(&f.turn.id, &first).unwrap();
        f.store.append_events(&f.turn.id, &second).unwrap();

        let loaded = f.store.load_thread(&f.thread.id).unwrap();
        assert_eq!(loaded.turns.len(), 1);
        let got = &loaded.turns[0].events;
        assert_eq!(got.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2, 3]);
        let events: Vec<_> = got.iter().map(|e| e.event.clone()).collect();
        let want: Vec<_> = first
            .iter()
            .chain(&second)
            .map(|e| e.event.clone())
            .collect();
        assert_eq!(events, want);
        assert_eq!(got[0].raw, first[0].raw);
        assert_eq!(got[1].raw, None);
    }

    // Fails if a second open re-runs or breaks migrations, or data is lost on reopen.
    #[test]
    fn reopening_is_idempotent_and_keeps_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hearth.db");
        let mut store = Store::open(&path).unwrap();
        store.add_project(Path::new("/p"), "p").unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.list_projects().unwrap().len(), 1);
    }

    // Fails if a database from a newer build is opened (and possibly corrupted).
    #[test]
    fn newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hearth.db");
        Store::open(&path).unwrap();
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", 99)
            .unwrap();
        assert!(matches!(
            Store::open(&path),
            Err(StoreError::SchemaTooNew(99))
        ));
    }

    // Fails if approvals, usage, settings or thread status do not round-trip.
    #[test]
    fn approvals_usage_settings_and_status_round_trip() {
        let mut f = fixture();
        let request = ApprovalRequest {
            id: ApprovalId::new(),
            tool: "run_shell_command".into(),
            title: "Run".into(),
            detail: "ls".into(),
            options: vec![ApprovalOption {
                option_id: "once".into(),
                label: "Allow once".into(),
                kind: ApprovalOptionKind::AllowOnce,
            }],
        };
        f.store
            .record_approval(&f.turn.id, &request, ApprovalDecision::AllowForThread)
            .unwrap();
        assert_eq!(
            f.store.list_approvals(&f.turn.id).unwrap(),
            vec![(request.clone(), ApprovalDecision::AllowForThread)]
        );

        let usage = TokenUsage {
            input_tokens: 5,
            output_tokens: 7,
            cache_read_tokens: Some(2),
            cache_write_tokens: None,
        };
        f.store.record_usage(&f.turn.id, &usage).unwrap();
        assert_eq!(f.store.get_usage(&f.turn.id).unwrap(), Some(usage));

        assert_eq!(f.store.get_setting("theme").unwrap(), None);
        f.store
            .set_setting("theme", &serde_json::json!("dark"))
            .unwrap();
        f.store
            .set_setting("theme", &serde_json::json!({"mode": "light"}))
            .unwrap();
        assert_eq!(
            f.store.get_setting("theme").unwrap(),
            Some(serde_json::json!({"mode": "light"}))
        );

        f.store
            .update_thread_status(&f.thread.id, ThreadStatus::Archived)
            .unwrap();
        let t = f.store.load_thread(&f.thread.id).unwrap().thread;
        assert_eq!(t.status, ThreadStatus::Archived);
        assert!(t.archived_at.is_some());
    }

    // Fails if foreign keys are off: a turn for a missing thread must be refused.
    #[test]
    fn foreign_keys_are_enforced() {
        let mut f = fixture();
        let session = AgentSessionId::new();
        assert!(
            f.store
                .start_turn(&ThreadId::new(), &session, TurnRole::User, None)
                .is_err()
        );
    }
}
