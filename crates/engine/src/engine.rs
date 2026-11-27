//! The engine: store, guard and live agent sessions behind the UI's operations.

use crate::agents::AgentLauncher;
use crate::agents::qwen::{DEFAULT_PASSTHROUGH, PASSTHROUGH_SETTING};
use crate::api::{
    EngineError, EventSink, Project, ThreadEventPayload, ThreadSnapshot, ThreadSummary,
    TurnSnapshot,
};
use crate::coalesce::coalesce;
use hearth_adapter_qwen::{AdapterItem, AdapterStream, QwenProcess};
use hearth_core::fsguard::FsGuard;
use hearth_core::store::{self, NewEvent, NewThread, Store, ThreadStatus, TurnRole};
use hearth_protocol::{
    AgentEvent, AgentId, ApprovalDecision, ApprovalId, ApprovalRequest, PermissionMode, ProjectId,
    StopReason, ThreadId, TurnId, UserInput,
};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::time::Instant;

/// Events are persisted and emitted at most this often (and at once on the
/// events that must not wait).
const FLUSH_EVERY: Duration = Duration::from_millis(16);
const NEW_THREAD_TITLE: &str = "New thread";

pub struct EngineConfig {
    /// `None` keeps the database in memory.
    pub db_path: Option<PathBuf>,
    pub home: PathBuf,
    /// Allowed roots besides the projects (the app's data dir).
    pub roots: Vec<PathBuf>,
    pub launcher: Arc<dyn AgentLauncher>,
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

struct Inner {
    store: Mutex<Store>,
    guard: Mutex<FsGuard>,
    home: PathBuf,
    sink: Arc<dyn EventSink>,
    launcher: Arc<dyn AgentLauncher>,
    sessions: tokio::sync::Mutex<HashMap<ThreadId, Arc<Live>>>,
    running: Mutex<HashSet<ThreadId>>,
    /// Next `seq` to hand out, per thread.
    seqs: Mutex<HashMap<ThreadId, u64>>,
    cancel_grace_ms: AtomicU64,
}

/// One live agent process and the approvals it is waiting on.
struct Live {
    process: QwenProcess,
    pending: Mutex<HashMap<ApprovalId, (ApprovalRequest, TurnId)>>,
    /// False until a turn has run on this session; an auth failure before
    /// that gets the setup hint.
    turn_done: AtomicBool,
    /// Counts turns on this session; a cancel names the turn it was for.
    turn_seq: AtomicU64,
    /// Set (with `force_end` notified) when a cancelled turn got no end from
    /// the agent in time; holds the number of the turn to end.
    force_seq: AtomicU64,
    force_end: tokio::sync::Notify,
}

/// How long after `session/cancel` the agent has to end the turn itself.
const CANCEL_GRACE: Duration = Duration::from_secs(3);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn thread_summary(store: &Store, t: &store::Thread) -> Result<ThreadSummary, EngineError> {
    let session = store.latest_agent_session(&t.id)?;
    Ok(ThreadSummary {
        id: t.id.clone(),
        project_id: t.project_id.clone(),
        title: t.title.clone(),
        agent: session
            .as_ref()
            .map_or(AgentId::QwenCode, |s| s.agent.clone()),
        model: session.map(|s| s.model).unwrap_or_default(),
    })
}

impl Engine {
    pub fn new(config: EngineConfig, sink: Arc<dyn EventSink>) -> Result<Self, EngineError> {
        let store = match &config.db_path {
            Some(p) => Store::open(p)?,
            None => Store::open_in_memory()?,
        };
        // A turn the app was closed during never got its end; close it now.
        let mut store = store;
        for (thread, turn) in store.unfinished_turns()? {
            store.append_events(
                &turn,
                &[
                    NewEvent {
                        event: AgentEvent::Error {
                            message: "Hearth closed during this turn".into(),
                        },
                        raw: None,
                    },
                    NewEvent {
                        event: AgentEvent::TurnEnd {
                            stop_reason: StopReason::Cancelled,
                        },
                        raw: None,
                    },
                ],
            )?;
            store.update_thread_status(&thread, ThreadStatus::Idle)?;
        }
        let mut roots = config.roots;
        roots.extend(
            store
                .list_projects()?
                .into_iter()
                .map(|p| PathBuf::from(p.path)),
        );
        Ok(Self {
            inner: Arc::new(Inner {
                store: Mutex::new(store),
                guard: Mutex::new(FsGuard::new(roots, config.home.clone())),
                home: config.home,
                sink,
                launcher: config.launcher,
                sessions: Default::default(),
                running: Default::default(),
                seqs: Default::default(),
                cancel_grace_ms: AtomicU64::new(CANCEL_GRACE.as_millis() as u64),
            }),
        })
    }

    pub fn list_projects(&self) -> Result<Vec<Project>, EngineError> {
        Ok(lock(&self.inner.store)
            .list_projects()?
            .into_iter()
            .map(|p| Project {
                id: p.id,
                name: p.name,
                path: p.path,
            })
            .collect())
    }

    /// Canonicalizes the path, refuses one the guard denies, adds the folder
    /// as an allowed root. Adding a known project returns it.
    pub fn add_project(&self, path: &str) -> Result<Project, EngineError> {
        let canon = Path::new(path)
            .canonicalize()
            .map_err(|e| EngineError(format!("cannot open {path}: {e}")))?;
        if !canon.is_dir() {
            return Err(EngineError(format!("{} is not a folder", canon.display())));
        }
        // The folder is not a root yet: ask a guard that has only this root,
        // so what is left to refuse is the denylist.
        FsGuard::new([canon.clone()], self.inner.home.clone())
            .try_check(&canon)
            .map_err(|d| EngineError(d.to_string()))?;
        let text = canon.to_string_lossy().into_owned();
        let mut store = lock(&self.inner.store);
        let project = match store.list_projects()?.into_iter().find(|p| p.path == text) {
            Some(p) => p,
            None => {
                let name = canon
                    .file_name()
                    .map_or_else(|| text.clone(), |n| n.to_string_lossy().into_owned());
                store.add_project(&canon, &name)?
            }
        };
        lock(&self.inner.guard).add_root(&canon);
        Ok(Project {
            id: project.id,
            name: project.name,
            path: project.path,
        })
    }

    pub fn list_threads(&self, project: &ProjectId) -> Result<Vec<ThreadSummary>, EngineError> {
        let store = lock(&self.inner.store);
        store
            .list_threads(project)?
            .iter()
            .filter(|t| t.status != ThreadStatus::Archived)
            .map(|t| thread_summary(&store, t))
            .collect()
    }

    /// The thread works in the project's folder (worktrees come later).
    pub fn create_thread(
        &self,
        project: &ProjectId,
        agent: AgentId,
    ) -> Result<ThreadSummary, EngineError> {
        if agent != AgentId::QwenCode {
            return Err(EngineError("only Qwen Code is available so far".into()));
        }
        let mut store = lock(&self.inner.store);
        let project = store
            .list_projects()?
            .into_iter()
            .find(|p| p.id == *project)
            .ok_or("unknown project")?;
        lock(&self.inner.guard)
            .try_check(Path::new(&project.path))
            .map_err(|d| EngineError(d.to_string()))?;
        let thread = store.create_thread(
            &project.id,
            NewThread {
                title: NEW_THREAD_TITLE,
                cwd: &project.path,
                ..Default::default()
            },
        )?;
        store.start_agent_session(&thread.id, &agent, "", PermissionMode::Default, None)?;
        thread_summary(&store, &thread)
    }

    /// The thread as stored; the agent's own replay is not used for display.
    pub fn load_thread(&self, id: &ThreadId) -> Result<ThreadSnapshot, EngineError> {
        let store = lock(&self.inner.store);
        let loaded = store.load_thread(id)?;
        let summary = thread_summary(&store, &loaded.thread)?;
        let mut turns: Vec<TurnSnapshot> = Vec::new();
        let mut last_seq = 0u64;
        let mut user_input: Option<UserInput> = None;
        let mut pending_user = false;
        for t in loaded.turns {
            match t.turn.role {
                TurnRole::User => {
                    if pending_user {
                        turns.push(snapshot_turn(
                            &summary,
                            &t.turn.id,
                            user_input.take(),
                            vec![],
                        ));
                    }
                    user_input = t.turn.input;
                    pending_user = true;
                }
                TurnRole::Assistant => {
                    last_seq += t.events.len() as u64;
                    let events = t.events.into_iter().map(|e| e.event).collect();
                    turns.push(snapshot_turn(
                        &summary,
                        &t.turn.id,
                        user_input.take(),
                        events,
                    ));
                    pending_user = false;
                }
            }
        }
        if pending_user {
            // A user message whose agent turn never started.
            turns.push(snapshot_turn(&summary, &TurnId::new(), user_input, vec![]));
        }
        Ok(ThreadSnapshot {
            thread: summary,
            turns,
            last_seq,
        })
    }

    /// Test hook: shortens the wait for the agent's own end of a cancelled turn.
    #[doc(hidden)]
    pub fn set_cancel_grace(&self, grace: Duration) {
        self.inner
            .cancel_grace_ms
            .store(grace.as_millis() as u64, Ordering::Relaxed);
    }

    /// Stores the user's turn and starts the agent's; events then stream to
    /// the sink. Returns the thread as it is now: the agent's model is known
    /// once its session has started.
    pub async fn send_message(
        &self,
        thread: &ThreadId,
        input: UserInput,
    ) -> Result<ThreadSummary, EngineError> {
        if !lock(&self.inner.running).insert(thread.clone()) {
            return Err("a turn is already running in this thread".into());
        }
        let started = self.start_turn(thread, input).await;
        if started.is_err() {
            lock(&self.inner.running).remove(thread);
        }
        started
    }

    async fn start_turn(
        &self,
        thread_id: &ThreadId,
        input: UserInput,
    ) -> Result<ThreadSummary, EngineError> {
        let (thread, session) = {
            let store = lock(&self.inner.store);
            let thread = store.load_thread(thread_id)?.thread;
            let session = store
                .latest_agent_session(thread_id)?
                .ok_or("thread has no agent session")?;
            (thread, session)
        };
        let live = self.live_session(&thread, &session).await?;
        let seq = live.turn_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let (turn, summary) = {
            let mut store = lock(&self.inner.store);
            let user = store.start_turn(thread_id, &session.id, TurnRole::User, None)?;
            store.set_turn_input(&user.id, &input)?;
            let turn = store.start_turn(thread_id, &session.id, TurnRole::Assistant, None)?;
            if thread.title == NEW_THREAD_TITLE {
                let UserInput::Text { text } = &input;
                let title: String = text.lines().next().unwrap_or("").chars().take(60).collect();
                if !title.trim().is_empty() {
                    store.set_thread_title(thread_id, title.trim())?;
                }
            }
            store.update_thread_status(thread_id, ThreadStatus::Running)?;
            let thread = store.load_thread(thread_id)?.thread;
            (turn, thread_summary(&store, &thread)?)
        };
        let stream = live.process.prompt(input);
        tokio::spawn(run_turn(
            self.inner.clone(),
            live,
            thread_id.clone(),
            turn.id,
            seq,
            stream,
        ));
        Ok(summary)
    }

    /// The thread's live session; spawns the agent (and resumes its stored
    /// session) the first time.
    async fn live_session(
        &self,
        thread: &store::Thread,
        session: &store::AgentSession,
    ) -> Result<Arc<Live>, EngineError> {
        let mut sessions = self.inner.sessions.lock().await;
        if let Some(live) = sessions.get(&thread.id) {
            return Ok(live.clone());
        }
        let cwd = PathBuf::from(&thread.cwd);
        lock(&self.inner.guard)
            .try_check(&cwd)
            .map_err(|d| EngineError(d.to_string()))?;
        let passthrough = self.passthrough_names()?;
        let launcher = self.inner.launcher.clone();
        let spawn_cwd = cwd.clone();
        let config =
            tokio::task::spawn_blocking(move || launcher.spawn_config(&spawn_cwd, &passthrough))
                .await
                .map_err(|e| EngineError(e.to_string()))??;
        let process = QwenProcess::spawn(config).map_err(|e| EngineError(e.to_string()))?;
        let fail =
            |e: hearth_adapter_qwen::AdapterError| EngineError(with_auth_hint(&e.to_string()));
        process.initialize().await.map_err(fail)?;
        let resumed = match &session.external_session_id {
            Some(id) => process
                .load_session(id, &cwd)
                .await
                .ok()
                .map(|l| (id.clone(), l.models)),
            None => None,
        };
        let had_session = resumed.is_some();
        let (external, models) = match resumed {
            Some(r) => r,
            None => process.new_session(&cwd).await.map_err(fail)?,
        };
        let model = models
            .current
            .or_else(|| models.available.first().map(|m| m.id.clone()))
            .unwrap_or_default();
        lock(&self.inner.store).update_agent_session(&session.id, &model, &external)?;
        let live = Arc::new(Live {
            process,
            pending: Default::default(),
            turn_done: AtomicBool::new(had_session),
            turn_seq: AtomicU64::new(0),
            force_seq: AtomicU64::new(0),
            force_end: tokio::sync::Notify::new(),
        });
        sessions.insert(thread.id.clone(), live.clone());
        Ok(live)
    }

    fn passthrough_names(&self) -> Result<Vec<String>, EngineError> {
        let stored = lock(&self.inner.store).get_setting(PASSTHROUGH_SETTING)?;
        Ok(match stored {
            Some(Value::Array(names)) => names
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
            _ => DEFAULT_PASSTHROUGH.map(String::from).to_vec(),
        })
    }

    /// Records the answer. `AllowForThread` also stores a rule for the thread;
    /// the agent still gets a one-shot answer.
    pub async fn answer_approval(
        &self,
        thread: &ThreadId,
        approval: &ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<(), EngineError> {
        let live = self.running_session(thread).await?;
        let (request, turn) = lock(&live.pending)
            .remove(approval)
            .ok_or("unknown or already answered approval")?;
        {
            let mut store = lock(&self.inner.store);
            store.record_approval(&turn, &request, decision)?;
            if decision == ApprovalDecision::AllowForThread {
                add_rule(&mut store, thread, &request)?;
            }
            store.update_thread_status(thread, ThreadStatus::Running)?;
        }
        live.process
            .answer_permission(&request, decision)
            .await
            .map_err(|e| EngineError(e.to_string()))
    }

    /// Cancels the running turn; it ends with a `turn.end` of `cancelled`.
    pub async fn cancel_turn(&self, thread: &ThreadId) -> Result<(), EngineError> {
        let Ok(live) = self.running_session(thread).await else {
            return Ok(());
        };
        lock(&live.pending).clear();
        live.process
            .cancel()
            .await
            .map_err(|e| EngineError(e.to_string()))?;
        // The agent should end the turn. If it does not, end it here.
        let (inner, thread, seq) = (
            self.inner.clone(),
            thread.clone(),
            live.turn_seq.load(Ordering::Relaxed),
        );
        let grace = Duration::from_millis(inner.cancel_grace_ms.load(Ordering::Relaxed));
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            if lock(&inner.running).contains(&thread)
                && live.turn_seq.load(Ordering::Relaxed) == seq
            {
                live.force_seq.store(seq, Ordering::Relaxed);
                live.force_end.notify_one();
            }
        });
        Ok(())
    }

    async fn running_session(&self, thread: &ThreadId) -> Result<Arc<Live>, EngineError> {
        self.inner
            .sessions
            .lock()
            .await
            .get(thread)
            .cloned()
            .ok_or_else(|| {
                EngineError(
                    "This approval can no longer be answered: its turn is not running.".into(),
                )
            })
    }
}

fn snapshot_turn(
    thread: &ThreadSummary,
    id: &TurnId,
    input: Option<UserInput>,
    events: Vec<AgentEvent>,
) -> TurnSnapshot {
    TurnSnapshot {
        turn_id: id.clone(),
        agent: thread.agent.clone(),
        model: thread.model.clone(),
        input: input.unwrap_or(UserInput::Text {
            text: String::new(),
        }),
        events,
    }
}

fn rules_key(thread: &ThreadId) -> String {
    format!("thread.{}.approvalRules", thread.0)
}

fn add_rule(
    store: &mut Store,
    thread: &ThreadId,
    request: &ApprovalRequest,
) -> Result<(), EngineError> {
    let key = rules_key(thread);
    let mut rules = match store.get_setting(&key)? {
        Some(Value::Array(r)) => r,
        _ => vec![],
    };
    let rule = json!({"tool": request.tool, "title": request.title});
    if !rules.contains(&rule) {
        rules.push(rule);
    }
    Ok(store.set_setting(&key, &Value::Array(rules))?)
}

fn rule_matches(store: &Store, thread: &ThreadId, request: &ApprovalRequest) -> bool {
    match store.get_setting(&rules_key(thread)) {
        Ok(Some(Value::Array(rules))) => rules
            .iter()
            .any(|r| r["tool"] == request.tool && r["title"] == request.title),
        _ => false,
    }
}

/// An authentication failure gets the way out appended.
fn with_auth_hint(message: &str) -> String {
    let lower = message.to_lowercase();
    let auth = [
        "auth",
        "api key",
        "api_key",
        "apikey",
        "unauthorized",
        "401",
        "credential",
        "log in",
        "login",
        "sign in",
    ];
    if auth.iter().any(|k| lower.contains(k)) {
        format!(
            "{message} Run `qwen` in your terminal to set up the provider key, or add the key's variable name in Settings."
        )
    } else {
        message.to_owned()
    }
}

fn is_urgent(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::TurnEnd { .. } | AgentEvent::ApprovalRequest { .. } | AgentEvent::Error { .. }
    )
}

/// What a turn has shown so far.
#[derive(Default)]
struct TurnState {
    ended: bool,
    errored: bool,
}

enum Step {
    Item(Option<AdapterItem>),
    Tick,
    /// The agent did not end a cancelled turn in time.
    ForceEnd,
}

/// Streams one turn: persists and emits in batches (every 16 ms, or at once
/// on `turn.end`, `approval.request`, `error`).
async fn run_turn(
    inner: Arc<Inner>,
    live: Arc<Live>,
    thread: ThreadId,
    turn: TurnId,
    seq: u64,
    mut stream: AdapterStream,
) {
    let mut buf: Vec<AdapterItem> = Vec::new();
    let mut deadline: Option<Instant> = None;
    let mut state = TurnState::default();
    loop {
        let step = match deadline {
            Some(d) => tokio::select! {
                item = stream.next() => Step::Item(item),
                () = tokio::time::sleep_until(d) => Step::Tick,
                () = live.force_end.notified() => Step::ForceEnd,
            },
            None => tokio::select! {
                item = stream.next() => Step::Item(item),
                () = live.force_end.notified() => Step::ForceEnd,
            },
        };
        match step {
            Step::Item(Some(item)) => {
                let urgent = is_urgent(&item.event);
                buf.push(item);
                deadline.get_or_insert_with(|| Instant::now() + FLUSH_EVERY);
                if urgent {
                    flush(
                        &inner,
                        &live,
                        &thread,
                        &turn,
                        &mut state,
                        std::mem::take(&mut buf),
                        false,
                    )
                    .await;
                    deadline = None;
                }
            }
            Step::Tick => {
                flush(
                    &inner,
                    &live,
                    &thread,
                    &turn,
                    &mut state,
                    std::mem::take(&mut buf),
                    false,
                )
                .await;
                deadline = None;
            }
            Step::ForceEnd => {
                if live.force_seq.load(Ordering::Relaxed) != seq {
                    continue;
                }
                // What arrived is kept; the late answer of the agent is dropped.
                buf.push(AdapterItem {
                    event: AgentEvent::TurnEnd {
                        stop_reason: StopReason::Cancelled,
                    },
                    raw: Value::Null,
                });
                flush(
                    &inner,
                    &live,
                    &thread,
                    &turn,
                    &mut state,
                    std::mem::take(&mut buf),
                    true,
                )
                .await;
                return;
            }
            Step::Item(None) => {
                flush(
                    &inner,
                    &live,
                    &thread,
                    &turn,
                    &mut state,
                    std::mem::take(&mut buf),
                    true,
                )
                .await;
                return;
            }
        }
    }
}

async fn flush(
    inner: &Arc<Inner>,
    live: &Arc<Live>,
    thread: &ThreadId,
    turn: &TurnId,
    state: &mut TurnState,
    items: Vec<AdapterItem>,
    last: bool,
) {
    let mut events = coalesce(items);
    let mut auto_answered = Vec::new();
    for e in &mut events {
        match &mut e.event {
            AgentEvent::ApprovalRequest { request } => {
                if rule_matches(&lock(&inner.store), thread, request) {
                    auto_answered.push(request.clone());
                    request.title = format!("{} (allowed for this thread)", request.title);
                } else {
                    lock(&live.pending).insert(request.id.clone(), (request.clone(), turn.clone()));
                    let _ = lock(&inner.store)
                        .update_thread_status(thread, ThreadStatus::AwaitingApproval);
                }
            }
            AgentEvent::Error { message } if !live.turn_done.load(Ordering::Relaxed) => {
                *message = with_auth_hint(message);
            }
            _ => {}
        }
    }
    if last && !state.ended && !events.iter().any(is_turn_end) {
        // The stream is over. The UI waits for a `turn.end`; an error ends the turn too.
        if !state.errored
            && !events
                .iter()
                .any(|e| matches!(e.event, AgentEvent::Error { .. }))
        {
            events.push(NewEvent {
                event: AgentEvent::Error {
                    message: "The agent stopped unexpectedly.".into(),
                },
                raw: None,
            });
        }
        events.push(NewEvent {
            event: AgentEvent::TurnEnd {
                stop_reason: StopReason::EndTurn,
            },
            raw: None,
        });
    }
    state.errored |= events
        .iter()
        .any(|e| matches!(e.event, AgentEvent::Error { .. }));
    let ended = events.iter().any(is_turn_end);
    let payloads = persist(inner, thread, turn, events);
    if ended {
        state.ended = true;
        live.turn_done.store(true, Ordering::Relaxed);
        let status = if state.errored {
            ThreadStatus::Error
        } else {
            ThreadStatus::Idle
        };
        let _ = lock(&inner.store).update_thread_status(thread, status);
        // Before the last emit: a client that sends again on `turn.end` must be let in.
        lock(&inner.running).remove(thread);
    }
    for p in payloads {
        inner.sink.emit(p);
    }
    for request in auto_answered {
        let _ = lock(&inner.store).record_approval(turn, &request, ApprovalDecision::AllowOnce);
        let _ = live
            .process
            .answer_permission(&request, ApprovalDecision::AllowOnce)
            .await;
    }
}

fn is_turn_end(e: &NewEvent) -> bool {
    matches!(e.event, AgentEvent::TurnEnd { .. })
}

/// Writes the batch and numbers it. A store failure is reported as an error
/// event in place of the batch, so the UI does not wait forever.
fn persist(
    inner: &Inner,
    thread: &ThreadId,
    turn: &TurnId,
    events: Vec<NewEvent>,
) -> Vec<ThreadEventPayload> {
    if events.is_empty() {
        return vec![];
    }
    let mut store = lock(&inner.store);
    let mut seqs = lock(&inner.seqs);
    let next = seqs
        .entry(thread.clone())
        .or_insert_with(|| store.thread_event_count(thread).unwrap_or(0).max(0) as u64 + 1);
    let events = match store.append_events(turn, &events) {
        Ok(()) => events,
        Err(e) => vec![NewEvent {
            event: AgentEvent::Error {
                message: format!("could not save events: {e}"),
            },
            raw: None,
        }],
    };
    events
        .into_iter()
        .map(|e| {
            let seq = *next;
            *next += 1;
            ThreadEventPayload {
                thread_id: thread.clone(),
                turn_id: turn.clone(),
                seq,
                event: e.event,
            }
        })
        .collect()
}
