//! The engine: store, guard and live agent sessions behind the UI's operations.

use crate::agents::{
    AgentItem, AgentSession, AgentStream, EnvPassthrough, Registration, StreamItem,
};
use crate::api::{
    AgentStatus, EngineError, EventSink, InstalledAgent, Project, ReviewTurn, ThreadEventPayload,
    ThreadModels, ThreadSnapshot, ThreadSummary, TurnSnapshot, WorktreeInfo, WorktreeReview,
};
use crate::coalesce::coalesce;
use hearth_core::fsguard::FsGuard;
use hearth_core::store::{self, NewEvent, NewThread, Store, ThreadStatus, TurnRole};
use hearth_protocol::{
    AgentEvent, AgentId, ApprovalDecision, ApprovalId, ApprovalRequest, ProjectId, StopReason,
    ThreadId, TurnId, UserInput,
};
use serde_json::{Value, json};
use std::collections::{HashMap, hash_map::Entry};
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
    pub agents: Vec<Registration>,
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

struct Inner {
    store: Mutex<Store>,
    guard: Mutex<FsGuard>,
    home: PathBuf,
    snapshot_parent: Option<PathBuf>,
    #[cfg(unix)]
    reviews: Mutex<HashMap<ThreadId, LiveReviewState>>,
    sink: Arc<dyn EventSink>,
    agents: Vec<Registration>,
    sessions: tokio::sync::Mutex<HashMap<ThreadId, Arc<Live>>>,
    work: Mutex<HashMap<ThreadId, ThreadWork>>,
    /// Next `seq` to hand out, per thread.
    seqs: Mutex<HashMap<ThreadId, u64>>,
    cancel_grace_ms: AtomicU64,
}

#[cfg(unix)]
struct LiveReviewEntry {
    id: String,
    root: PathBuf,
    gitdir: PathBuf,
    common: PathBuf,
    review: Arc<hearth_core::snapshot::LiveReview>,
}

#[cfg(unix)]
enum LiveReviewState {
    Pending(String),
    Capturing(String),
    Ready(LiveReviewEntry),
}

#[cfg(unix)]
impl LiveReviewState {
    fn id(&self) -> &str {
        match self {
            Self::Pending(id) | Self::Capturing(id) => id,
            Self::Ready(entry) => &entry.id,
        }
    }
}

#[cfg(unix)]
fn retire_review(review: Option<LiveReviewState>) {
    if let Some(LiveReviewState::Ready(entry)) = review {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn_blocking(move || drop(entry));
        } else {
            std::thread::spawn(move || drop(entry));
        }
    }
}

#[cfg(unix)]
struct ReviewReservation<'a> {
    inner: &'a Inner,
    thread: &'a ThreadId,
    id: String,
}

#[cfg(unix)]
impl Drop for ReviewReservation<'_> {
    fn drop(&mut self) {
        let mut reviews = lock(&self.inner.reviews);
        if matches!(reviews.get(self.thread), Some(LiveReviewState::Capturing(id)) if id == &self.id)
        {
            reviews.remove(self.thread);
        }
    }
}

/// One live agent process and the approvals it is waiting on.
struct Live {
    process: Arc<dyn AgentSession>,
    model: Mutex<LiveModel>,
    remembers_approvals: bool,
    starting: tokio::sync::Mutex<()>,
    pending: Mutex<HashMap<ApprovalId, (ApprovalRequest, TurnId)>>,
}

#[derive(Default)]
struct LiveModel {
    actual: Option<String>,
    external_id: Option<String>,
    applied_choice: Option<String>,
}

enum ThreadWork {
    Turn(Arc<RunningTurn>),
    Models,
}

struct ModelWork<'a>(&'a Inner, &'a ThreadId);

impl Drop for ModelWork<'_> {
    fn drop(&mut self) {
        lock(&self.0.work).remove(self.1);
    }
}

#[derive(Default)]
struct RunningTurn {
    cancel_requested: AtomicBool,
    submitted: AtomicBool,
    force_end: tokio::sync::Notify,
    finished: tokio::sync::Notify,
}

enum TurnStart {
    Cancelled,
    Stream(Result<Box<dyn AgentStream>, String>),
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
        branch: t.branch.clone(),
        archived: t.status == ThreadStatus::Archived,
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
        let snapshot_parent = config
            .db_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(Path::to_path_buf);
        #[cfg(unix)]
        if let Some(parent) = snapshot_parent.clone() {
            let home = config.home.clone();
            std::thread::spawn(move || {
                let guard = FsGuard::new([parent.clone()], home);
                let _ = hearth_core::snapshot::recover_live_reviews(&guard, &parent);
            });
        }
        let mut roots = config.roots;
        for project in store.list_projects()? {
            let project_path = PathBuf::from(&project.path);
            for thread in store.list_threads(&project.id)? {
                if let Some(path) = thread.worktree_path.as_deref()
                    && thread.cwd == path
                    && let Ok(path) = hearth_core::git::validate_in(
                        &project_path,
                        &config.home,
                        Path::new(path),
                        thread.worktree_parent.as_deref().map(Path::new),
                    )
                {
                    roots.push(path);
                }
            }
            roots.push(project_path);
        }
        Ok(Self {
            inner: Arc::new(Inner {
                store: Mutex::new(store),
                guard: Mutex::new(FsGuard::new(roots, config.home.clone())),
                home: config.home,
                snapshot_parent,
                #[cfg(unix)]
                reviews: Default::default(),
                sink,
                agents: config.agents,
                sessions: Default::default(),
                work: Default::default(),
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
            .map(|t| thread_summary(&store, t))
            .collect()
    }

    pub async fn list_agents(&self) -> Vec<InstalledAgent> {
        let mut installed = Vec::with_capacity(self.inner.agents.len());
        for registered in &self.inner.agents {
            installed.push(InstalledAgent {
                id: registered.id.clone(),
                label: registered.label,
                status: registered.adapter.availability().await,
                remembers_approvals: registered.adapter.remembers_approvals(),
            });
        }
        installed
    }

    fn registration(&self, agent: &AgentId) -> Result<&Registration, EngineError> {
        self.inner
            .agents
            .iter()
            .find(|registered| &registered.id == agent)
            .ok_or_else(|| EngineError("This agent is not supported.".into()))
    }

    pub fn worktree_info(&self, project: &ProjectId) -> Result<WorktreeInfo, EngineError> {
        let project = lock(&self.inner.store)
            .list_projects()?
            .into_iter()
            .find(|p| p.id == *project)
            .ok_or("unknown project")?;
        lock(&self.inner.guard)
            .try_check(Path::new(&project.path))
            .map_err(|e| EngineError(e.to_string()))?;
        let path = Path::new(&project.path);
        match hearth_core::git::project_root(path, &self.inner.home) {
            Ok(_) => Ok(WorktreeInfo {
                is_git: true,
                base: hearth_core::git::base(path, &self.inner.home).ok(),
                reason: None,
            }),
            Err(reason) => Ok(WorktreeInfo {
                is_git: false,
                base: None,
                reason: (reason != "This project has no Git metadata.").then_some(reason),
            }),
        }
    }

    pub async fn create_thread(
        &self,
        project: &ProjectId,
        agent: AgentId,
    ) -> Result<ThreadSummary, EngineError> {
        self.create_thread_in(project, agent, None).await
    }

    pub async fn create_thread_in(
        &self,
        project: &ProjectId,
        agent: AgentId,
        base: Option<String>,
    ) -> Result<ThreadSummary, EngineError> {
        self.create_thread_in_parent(project, agent, base, None)
            .await
    }

    pub async fn create_thread_in_parent(
        &self,
        project: &ProjectId,
        agent: AgentId,
        base: Option<String>,
        parent: Option<String>,
    ) -> Result<ThreadSummary, EngineError> {
        let registered = self.registration(&agent)?;
        match registered.adapter.availability().await {
            AgentStatus::Ready => {}
            AgentStatus::Missing => {
                return Err(EngineError(format!("{} was not found.", registered.label)));
            }
            AgentStatus::Unsupported { version } => {
                return Err(EngineError(format!(
                    "{} version {version} is unsupported.",
                    registered.label
                )));
            }
        }
        let project = lock(&self.inner.store)
            .list_projects()?
            .into_iter()
            .find(|p| p.id == *project)
            .ok_or("unknown project")?;
        lock(&self.inner.guard)
            .try_check(Path::new(&project.path))
            .map_err(|e| EngineError(e.to_string()))?;
        let worktree = if let Some(base) = base {
            let path = PathBuf::from(&project.path);
            let home = self.inner.home.clone();
            let selected = parent.clone();
            Some(
                tokio::task::spawn_blocking(move || {
                    hearth_core::git::create_in(
                        &path,
                        &home,
                        &base,
                        selected.as_deref().map(Path::new),
                    )
                })
                .await
                .map_err(|e| EngineError(e.to_string()))?
                .map_err(EngineError)?,
            )
        } else {
            None
        };
        let cwd = worktree.as_ref().map_or_else(
            || project.path.clone(),
            |w| w.path.to_string_lossy().into_owned(),
        );
        let result = {
            let mut store = lock(&self.inner.store);
            store
                .create_thread_with_session(
                    &project.id,
                    NewThread {
                        title: NEW_THREAD_TITLE,
                        cwd: &cwd,
                        worktree_path: worktree.as_ref().map(|_| cwd.as_str()),
                        worktree_parent: worktree.as_ref().and(parent.as_deref()),
                        branch: worktree.as_ref().map(|w| w.branch.as_str()),
                        base_ref: worktree.as_ref().map(|w| w.base.as_str()),
                    },
                    &agent,
                )
                .map_err(EngineError::from)
                .map(|thread| ThreadSummary {
                    id: thread.id,
                    project_id: thread.project_id,
                    title: thread.title,
                    agent: agent.clone(),
                    model: String::new(),
                    branch: thread.branch,
                    archived: false,
                })
        };
        match result {
            Ok(summary) => {
                if let Some(worktree) = worktree {
                    let project_path = PathBuf::from(&project.path);
                    let path = worktree.path.clone();
                    let home = self.inner.home.clone();
                    let selected = parent.clone();
                    let root = tokio::task::spawn_blocking(move || {
                        hearth_core::git::validate_in(
                            &project_path,
                            &home,
                            &path,
                            selected.as_deref().map(Path::new),
                        )
                    })
                    .await
                    .map_err(|e| EngineError(e.to_string()))?
                    .map_err(|e| {
                        EngineError(format!(
                            "Thread was saved but its worktree cannot be opened at {}: {e}",
                            worktree.path.display()
                        ))
                    })?;
                    lock(&self.inner.guard).add_root(&root);
                }
                Ok(summary)
            }
            Err(error) => {
                if let Some(worktree) = worktree {
                    return Err(EngineError(format!(
                        "{error}; thread was not saved. The worktree at {} and branch {} may be removed after inspection.",
                        worktree.path.display(),
                        worktree.branch
                    )));
                }
                Err(EngineError(error.to_string()))
            }
        }
    }

    /// Lists reviewable assistant turns without loading their events.
    pub fn review_turns(&self, thread: &ThreadId) -> Result<Vec<ReviewTurn>, EngineError> {
        let turns = lock(&self.inner.store).review_turns(thread)?;
        Ok(turns
            .into_iter()
            .enumerate()
            .map(|(index, (turn_id, input, available))| {
                let label = match input {
                    Some(UserInput::Text { text }) if !text.trim().is_empty() => text
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .chars()
                        .take(80)
                        .collect(),
                    _ => format!("Turn {}", index + 1),
                };
                ReviewTurn {
                    turn_id,
                    label,
                    available: available && self.inner.snapshot_parent.is_some(),
                }
            })
            .collect())
    }

    /// Reserves an identity before capture so a closing panel can cancel it.
    #[cfg(unix)]
    pub fn start_worktree_review(&self, thread: &ThreadId) -> String {
        let id = ThreadId::new().0;
        let retired =
            lock(&self.inner.reviews).insert(thread.clone(), LiveReviewState::Pending(id.clone()));
        retire_review(retired);
        id
    }

    /// Captures a worktree against the immutable base saved with the thread.
    #[cfg(unix)]
    pub async fn worktree_diff_files(
        &self,
        thread: &ThreadId,
        id: &str,
    ) -> Result<Option<WorktreeReview>, EngineError> {
        {
            let mut reviews = lock(&self.inner.reviews);
            if !matches!(reviews.get(thread), Some(LiveReviewState::Pending(pending)) if pending == id)
            {
                return Ok(None);
            }
            reviews.insert(thread.clone(), LiveReviewState::Capturing(id.to_owned()));
        }
        let id = id.to_owned();
        let _reservation = ReviewReservation {
            inner: &self.inner,
            thread,
            id: id.clone(),
        };
        let (saved, project) = {
            let store = lock(&self.inner.store);
            let saved = store.get_thread(thread)?;
            let project = store
                .list_projects()?
                .into_iter()
                .find(|p| p.id == saved.project_id)
                .ok_or("unknown project")?;
            (saved, project)
        };
        let Some(base) = saved.base_ref.clone() else {
            return Ok(None);
        };
        let Some(parent) = self.inner.snapshot_parent.clone() else {
            return Ok(None);
        };
        if saved.worktree_path.as_deref() != Some(saved.cwd.as_str()) {
            return Ok(None);
        }
        let root = PathBuf::from(&saved.cwd);
        if lock(&self.inner.guard)
            .try_check(&root)
            .ok()
            .map(|p| p.as_path().to_path_buf())
            .as_deref()
            != Some(root.as_path())
        {
            return Ok(None);
        }
        let home = self.inner.home.clone();
        let project = PathBuf::from(project.path);
        let captured = tokio::task::spawn_blocking(move || -> Option<_> {
            let owner = FsGuard::new([project.clone()], home.clone());
            let guard = FsGuard::new([project.clone(), root.clone(), parent.clone()], home);
            let (_, common) = owner.git_metadata_dirs(&project, &owner).ok()??;
            let (gitdir, checkout_common) = guard.git_metadata_dirs(&root, &owner).ok()??;
            if common != checkout_common {
                return None;
            }
            let parent = guard.try_check(&parent).ok()?;
            let review = hearth_core::snapshot::LiveReview::open(
                &guard,
                parent.as_path(),
                &root,
                &gitdir,
                &common,
                &base,
            )
            .ok()?;
            let files = review.files().ok()?;
            Some((root, gitdir, common, review, files))
        })
        .await;
        let mut reviews = lock(&self.inner.reviews);
        if !matches!(reviews.get(thread), Some(LiveReviewState::Capturing(pending)) if pending == &id)
        {
            drop(reviews);
            if let Ok(Some((_, _, _, review, _))) = captured {
                tokio::task::spawn_blocking(move || drop(review));
            }
            return Ok(None);
        }
        let Some((root, gitdir, common, review, files)) =
            captured.map_err(|e| EngineError(e.to_string()))?
        else {
            reviews.remove(thread);
            return Ok(None);
        };
        reviews.insert(
            thread.clone(),
            LiveReviewState::Ready(LiveReviewEntry {
                id: id.clone(),
                root,
                gitdir,
                common,
                review: Arc::new(review),
            }),
        );
        Ok(Some(WorktreeReview { id, files }))
    }

    /// Reads a file from the exact captured review; a refresh invalidates its ID.
    #[cfg(unix)]
    pub async fn worktree_diff_file(
        &self,
        thread: &ThreadId,
        review_id: &str,
        file_id: &str,
    ) -> Result<Option<hearth_core::snapshot::FileDiff>, EngineError> {
        let (root, gitdir, common, review) = {
            let reviews = lock(&self.inner.reviews);
            let Some(LiveReviewState::Ready(entry)) =
                reviews.get(thread).filter(|entry| entry.id() == review_id)
            else {
                return Ok(None);
            };
            (
                entry.root.clone(),
                entry.gitdir.clone(),
                entry.common.clone(),
                entry.review.clone(),
            )
        };
        let (saved, project) = {
            let store = lock(&self.inner.store);
            let saved = store.get_thread(thread)?;
            let project = store
                .list_projects()?
                .into_iter()
                .find(|p| p.id == saved.project_id)
                .ok_or("unknown project")?;
            (saved, project)
        };
        if saved.worktree_path.as_deref() != Some(saved.cwd.as_str())
            || Path::new(&saved.cwd) != root
            || lock(&self.inner.guard)
                .try_check(&root)
                .ok()
                .map(|p| p.as_path().to_path_buf())
                .as_deref()
                != Some(root.as_path())
        {
            tokio::task::spawn_blocking(move || drop(review));
            return Ok(None);
        }
        let home = self.inner.home.clone();
        let file_id = file_id.to_owned();
        let result = tokio::task::spawn_blocking(move || {
            let project = PathBuf::from(project.path);
            let owner = FsGuard::new([project.clone()], home.clone());
            let guard = FsGuard::new([root.clone()], home);
            let owner_common = owner
                .git_metadata_dirs(&project, &owner)
                .ok()
                .flatten()
                .map(|(_, common)| common);
            let checkout_metadata = guard.git_metadata_dirs(&root, &owner).ok().flatten();
            if owner_common.as_ref() != Some(&common)
                || checkout_metadata.as_ref() != Some(&(gitdir, common))
            {
                return Ok(None);
            }
            review.file_diff(&file_id).map(Some).map_err(EngineError)
        })
        .await
        .map_err(|e| EngineError(e.to_string()))?;
        if !lock(&self.inner.reviews)
            .get(thread)
            .is_some_and(|entry| entry.id() == review_id)
        {
            return Ok(None);
        }
        result
    }

    /// Drops a captured review when its panel closes or switches scope.
    #[cfg(unix)]
    pub async fn release_worktree_review(&self, thread: &ThreadId, review_id: &str) {
        let retired = {
            let mut reviews = lock(&self.inner.reviews);
            if reviews
                .get(thread)
                .is_some_and(|entry| entry.id() == review_id)
            {
                reviews.remove(thread)
            } else {
                None
            }
        };
        if let Some(LiveReviewState::Ready(entry)) = retired {
            let _ = tokio::task::spawn_blocking(move || drop(entry)).await;
        }
    }

    /// Lists files from retained trees; None means the snapshots are unavailable.
    pub async fn turn_diff_files(
        &self,
        thread: &ThreadId,
        turn: &TurnId,
    ) -> Result<Option<Vec<hearth_core::snapshot::DiffFile>>, EngineError> {
        let (before, after) = lock(&self.inner.store)
            .turn_tree_pair(thread, turn)?
            .ok_or("unknown assistant turn")?;
        let (Some(before), Some(after), Some(parent)) =
            (before, after, self.inner.snapshot_parent.clone())
        else {
            return Ok(None);
        };
        let home = self.inner.home.clone();
        Ok(tokio::task::spawn_blocking(move || {
            let guard = FsGuard::new([parent.clone()], home);
            let data = guard.try_check(&parent).ok()?.as_path().join("snapshots");
            hearth_core::snapshot::files(&guard, &data, &before, &after).ok()
        })
        .await
        .ok()
        .flatten())
    }

    /// Reads a previously listed file from retained trees on a blocking task.
    #[cfg(unix)]
    pub async fn turn_diff_file(
        &self,
        thread: &ThreadId,
        turn: &TurnId,
        id: &str,
    ) -> Result<Option<hearth_core::snapshot::FileDiff>, EngineError> {
        let (before, after) = lock(&self.inner.store)
            .turn_tree_pair(thread, turn)?
            .ok_or("unknown assistant turn")?;
        let (Some(before), Some(after), Some(parent)) =
            (before, after, self.inner.snapshot_parent.clone())
        else {
            return Ok(None);
        };
        let home = self.inner.home.clone();
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            let guard = FsGuard::new([parent.clone()], home);
            let data = guard
                .try_check(&parent)
                .map_err(|e| EngineError(e.to_string()))?
                .as_path()
                .join("snapshots");
            hearth_core::snapshot::file_diff(&guard, &data, &before, &after, &id)
                .map(Some)
                .map_err(EngineError)
        })
        .await
        .map_err(|e| EngineError(e.to_string()))?
    }

    /// Returns the retained diff for an assistant turn, or None if unavailable.
    pub async fn turn_diff(
        &self,
        thread: &ThreadId,
        turn: &TurnId,
    ) -> Result<Option<Vec<u8>>, EngineError> {
        let (before, after) = lock(&self.inner.store)
            .turn_tree_pair(thread, turn)?
            .ok_or("unknown assistant turn")?;
        let (Some(before), Some(after), Some(parent)) =
            (before, after, self.inner.snapshot_parent.clone())
        else {
            return Ok(None);
        };
        let home = self.inner.home.clone();
        Ok(tokio::task::spawn_blocking(move || {
            let guard = FsGuard::new([parent.clone()], home);
            let data = guard.try_check(&parent).ok()?.as_path().join("snapshots");
            hearth_core::snapshot::diff(&guard, &data, &before, &after).ok()
        })
        .await
        .ok()
        .flatten())
    }

    /// The thread as stored; the agent's own replay is not used for display.
    pub fn load_thread(&self, id: &ThreadId) -> Result<ThreadSnapshot, EngineError> {
        let store = lock(&self.inner.store);
        let loaded = store.load_thread(id)?;
        let summary = thread_summary(&store, &loaded.thread)?;
        let mut turns: Vec<TurnSnapshot> = Vec::new();
        let mut last_seq = 0u64;
        let mut pending_user: Option<store::LoadedTurn> = None;
        for turn in loaded.turns {
            match turn.turn.role {
                TurnRole::User => {
                    if let Some(previous) = pending_user.replace(turn) {
                        turns.push(snapshot_turn(previous, None));
                    }
                }
                TurnRole::Assistant => {
                    last_seq += turn.events.len() as u64;
                    turns.push(snapshot_turn(
                        turn,
                        pending_user.take().and_then(|user| user.turn.input),
                    ));
                }
            }
        }
        if let Some(user) = pending_user {
            turns.push(snapshot_turn(user, None));
        }
        Ok(ThreadSnapshot {
            thread: summary,
            turns,
            last_seq,
        })
    }

    pub async fn archive_thread(&self, id: &ThreadId) -> Result<ThreadSummary, EngineError> {
        let _work = self.reserve_models(id)?;
        let summary = {
            let mut store = lock(&self.inner.store);
            store.get_thread(id)?;
            store.update_thread_status(id, ThreadStatus::Archived)?;
            thread_summary(&store, &store.get_thread(id)?)?
        };
        self.inner.sessions.lock().await.remove(id);
        Ok(summary)
    }

    pub fn restore_thread(&self, id: &ThreadId) -> Result<ThreadSummary, EngineError> {
        let _work = self.reserve_models(id)?;
        let mut store = lock(&self.inner.store);
        let thread = store.get_thread(id)?;
        if thread.status == ThreadStatus::Archived {
            store.update_thread_status(id, ThreadStatus::Idle)?;
        }
        thread_summary(&store, &store.get_thread(id)?)
    }

    pub async fn list_thread_models(&self, thread: &ThreadId) -> Result<ThreadModels, EngineError> {
        let _work = self.reserve_models(thread)?;
        let (thread, session, selected) = self.thread_session(thread)?;
        let live = self.live_session(&thread, &session).await?;
        let choices = live.process.models().to_vec();
        let current = lock(&live.model)
            .actual
            .clone()
            .or_else(|| (!session.model.is_empty()).then_some(session.model));
        Ok(ThreadModels {
            choices,
            selected,
            current,
        })
    }

    pub async fn set_thread_model(
        &self,
        id: &ThreadId,
        choice: &str,
    ) -> Result<ThreadSummary, EngineError> {
        let _work = self.reserve_models(id)?;
        let (thread, session, _) = self.thread_session(id)?;
        let live = self.live_session(&thread, &session).await?;
        let actual = self.apply_model(&thread.id, &live, choice).await?;
        let persisted = {
            let state = lock(&live.model);
            lock(&self.inner.store).select_model(
                id,
                &session.id,
                choice,
                &actual,
                state.external_id.as_deref(),
            )
        };
        if let Err(error) = persisted {
            evict_session(&self.inner, &live, id).await;
            return Err(error.into());
        }
        thread_summary(&lock(&self.inner.store), &thread)
    }

    fn reserve_models<'a>(&'a self, thread: &'a ThreadId) -> Result<ModelWork<'a>, EngineError> {
        match lock(&self.inner.work).entry(thread.clone()) {
            Entry::Occupied(_) => {
                Err("Wait for the current turn or model operation to finish.".into())
            }
            Entry::Vacant(entry) => {
                entry.insert(ThreadWork::Models);
                Ok(ModelWork(&self.inner, thread))
            }
        }
    }

    fn thread_session(
        &self,
        id: &ThreadId,
    ) -> Result<(store::Thread, store::AgentSession, Option<String>), EngineError> {
        let store = lock(&self.inner.store);
        let thread = store.get_thread(id)?;
        if thread.status == ThreadStatus::Archived {
            return Err("Restore this thread before sending or changing models.".into());
        }
        let session = store
            .latest_agent_session(id)?
            .ok_or("thread has no agent session")?;
        Ok((thread, session, store.selected_model(id)?))
    }

    async fn apply_model(
        &self,
        thread: &ThreadId,
        live: &Arc<Live>,
        id: &str,
    ) -> Result<String, EngineError> {
        let choice = live
            .process
            .models()
            .iter()
            .find(|choice| choice.id == id)
            .ok_or("This model is no longer available. Choose another model.")?;
        let switched =
            tokio::time::timeout(Duration::from_secs(20), live.process.set_model(choice))
                .await
                .unwrap_or_else(|_| Err("The agent did not acknowledge the model change.".into()));
        match switched {
            Ok(actual) => {
                let mut state = lock(&live.model);
                state.applied_choice = Some(choice.id.clone());
                state.actual = Some(actual.clone());
                Ok(actual)
            }
            Err(error) => {
                // A failed transport may have lost the acknowledgement after switching.
                evict_session(&self.inner, live, thread).await;
                Err(EngineError(error))
            }
        }
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
        let running = Arc::new(RunningTurn::default());
        match lock(&self.inner.work).entry(thread.clone()) {
            Entry::Occupied(_) => {
                return Err("Wait for the current turn or model operation to finish.".into());
            }
            Entry::Vacant(entry) => entry.insert(ThreadWork::Turn(running.clone())),
        };
        let started = self.start_turn(thread, input, running).await;
        if started.is_err() {
            lock(&self.inner.work).remove(thread);
        }
        started
    }

    async fn start_turn(
        &self,
        thread_id: &ThreadId,
        input: UserInput,
        running: Arc<RunningTurn>,
    ) -> Result<ThreadSummary, EngineError> {
        let (thread, session, selected) = self.thread_session(thread_id)?;
        let live = self.live_session(&thread, &session).await?;
        let starting = live.starting.lock().await;
        if let Some(selected) = selected.as_deref()
            && !running.cancel_requested.load(Ordering::Acquire)
            && lock(&live.model).applied_choice.as_deref() != Some(selected)
        {
            self.apply_model(thread_id, &live, selected).await?;
        }
        let before = capture_turn_tree(self.inner.clone(), thread.clone()).await;
        let (turn, summary) = {
            let state = lock(&live.model);
            let mut store = lock(&self.inner.store);
            let session_id = match &state.actual {
                Some(model) => {
                    store.prepare_turn_session(&session, model, state.external_id.as_deref())?
                }
                None => session.id,
            };
            let user =
                store.start_turn(thread_id, &session_id, TurnRole::User, before.as_deref())?;
            store.set_turn_input(&user.id, &input)?;
            let turn = store.start_turn(thread_id, &session_id, TurnRole::Assistant, None)?;
            if thread.title == NEW_THREAD_TITLE {
                let UserInput::Text { text } = &input;
                let title: String = text.lines().next().unwrap_or("").chars().take(60).collect();
                if !title.trim().is_empty() {
                    store.set_thread_title(thread_id, title.trim())?;
                }
            }
            store.update_thread_status(thread_id, ThreadStatus::Running)?;
            let thread = store.get_thread(thread_id)?;
            (turn, thread_summary(&store, &thread)?)
        };
        let start = if running.cancel_requested.load(Ordering::Acquire) {
            TurnStart::Cancelled
        } else {
            let stream = live.process.prompt(input).await;
            running.submitted.store(stream.is_ok(), Ordering::Release);
            TurnStart::Stream(stream)
        };
        tokio::spawn(run_turn(
            self.inner.clone(),
            live.clone(),
            thread_id.clone(),
            turn,
            summary.model.clone(),
            running,
            start,
        ));
        drop(starting);
        Ok(summary)
    }

    /// The thread's live session; spawns the agent (and resumes its stored
    /// session) the first time.
    async fn live_session(
        &self,
        thread: &store::Thread,
        session: &store::AgentSession,
    ) -> Result<Arc<Live>, EngineError> {
        if let Some(live) = self.inner.sessions.lock().await.get(&thread.id).cloned() {
            return Ok(live);
        }
        let cwd = PathBuf::from(&thread.cwd);
        lock(&self.inner.guard)
            .try_check(&cwd)
            .map_err(|d| EngineError(d.to_string()))?;
        let registered = self.registration(&session.agent)?;
        let passthrough = self.passthrough_names(registered.adapter.passthrough())?;
        let opened = tokio::time::timeout(
            Duration::from_secs(30),
            registered.adapter.open_session(
                &cwd,
                session.external_session_id.as_deref(),
                &passthrough,
            ),
        )
        .await
        .map_err(|_| EngineError("The agent did not open a session.".into()))?
        .map_err(EngineError)?;
        let live = Arc::new(Live {
            process: opened.session,
            model: Mutex::new(LiveModel {
                actual: opened.model,
                external_id: opened
                    .external_id
                    .or_else(|| session.external_session_id.clone()),
                applied_choice: None,
            }),
            remembers_approvals: registered.adapter.remembers_approvals(),
            starting: Default::default(),
            pending: Default::default(),
        });
        self.inner
            .sessions
            .lock()
            .await
            .insert(thread.id.clone(), live.clone());
        Ok(live)
    }

    fn passthrough_names(
        &self,
        policy: Option<EnvPassthrough>,
    ) -> Result<Vec<String>, EngineError> {
        let Some(policy) = policy else {
            return Ok(Vec::new());
        };
        let stored = lock(&self.inner.store).get_setting(policy.key)?;
        Ok(match stored {
            Some(Value::Array(names)) => names
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
            _ => policy
                .defaults
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
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
        if decision == ApprovalDecision::AllowForThread && !live.remembers_approvals {
            return Err("This approval can only be allowed once.".into());
        }
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
            .answer_approval(&request, decision)
            .await
            .map_err(|e| EngineError(e.to_string()))
    }

    /// Cancels the running turn; it ends with a `turn.end` of `cancelled`.
    pub async fn cancel_turn(&self, thread: &ThreadId) -> Result<(), EngineError> {
        let running = match lock(&self.inner.work).get(thread) {
            Some(ThreadWork::Turn(running)) => running.clone(),
            _ => return Ok(()),
        };
        if running.cancel_requested.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let Ok(live) = self.running_session(thread).await else {
            return Ok(());
        };
        let _starting = live.starting.lock().await;
        if !running.submitted.load(Ordering::Acquire)
            || !matches!(lock(&self.inner.work).get(thread),
                Some(ThreadWork::Turn(current)) if Arc::ptr_eq(current, &running))
        {
            return Ok(());
        }
        lock(&live.pending).clear();

        // The notification belongs to this turn, even if the next turn has started by then.
        let grace = Duration::from_millis(self.inner.cancel_grace_ms.load(Ordering::Relaxed));
        let deadline_turn = running.clone();
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            deadline_turn.force_end.notify_one();
        });
        tokio::select! {
            biased;
            () = running.finished.notified() => Ok(()),
            result = tokio::time::timeout(grace, live.process.cancel()) => {
                result.unwrap_or(Ok(())).map_err(EngineError)
            }
        }
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

fn snapshot_turn(turn: store::LoadedTurn, input: Option<UserInput>) -> TurnSnapshot {
    TurnSnapshot {
        turn_id: turn.turn.id,
        agent: turn.agent,
        model: turn.model,
        input: input.or(turn.turn.input).unwrap_or(UserInput::Text {
            text: String::new(),
        }),
        events: turn.events.into_iter().map(|event| event.event).collect(),
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
    model: Option<String>,
}

enum Step {
    Item(Option<StreamItem>),
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
    stored_turn: store::Turn,
    model: String,
    running: Arc<RunningTurn>,
    start: TurnStart,
) {
    let turn = stored_turn.id.clone();
    let mut stream = match start {
        TurnStart::Stream(Ok(stream)) => stream,
        TurnStart::Cancelled => {
            flush(
                &inner,
                &live,
                &thread,
                &turn,
                &mut TurnState::default(),
                vec![cancelled()],
                true,
            )
            .await;
            return;
        }
        TurnStart::Stream(Err(message)) => {
            fail_stream(
                &inner,
                &live,
                &thread,
                &turn,
                &mut TurnState::default(),
                vec![],
                message,
            )
            .await;
            return;
        }
    };
    let mut buf: Vec<AgentItem> = Vec::new();
    let mut deadline: Option<Instant> = None;
    let mut state = TurnState {
        model: (!model.is_empty()).then_some(model),
        ..Default::default()
    };
    loop {
        let step = match deadline {
            Some(d) => tokio::select! {
                item = std::future::poll_fn(|cx| stream.poll_next(cx)) => Step::Item(item),
                () = tokio::time::sleep_until(d) => Step::Tick,
                () = running.force_end.notified() => Step::ForceEnd,
            },
            None => tokio::select! {
                item = std::future::poll_fn(|cx| stream.poll_next(cx)) => Step::Item(item),
                () = running.force_end.notified() => Step::ForceEnd,
            },
        };
        match step {
            Step::Item(Some(StreamItem::Session { external_id, model })) => {
                if let Err(message) = save_session(&inner, &stored_turn, &model, &external_id) {
                    fail_stream(&inner, &live, &thread, &turn, &mut state, buf, message).await;
                    return;
                }
                let mut native = lock(&live.model);
                native.actual = Some(model.clone());
                native.external_id = Some(external_id);
                state.model = Some(model);
            }
            Step::Item(Some(StreamItem::Failed(message))) => {
                fail_stream(&inner, &live, &thread, &turn, &mut state, buf, message).await;
                return;
            }
            Step::Item(Some(StreamItem::Event(item))) => {
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
                    if state.ended {
                        return;
                    }
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
                if live.process.reopen_after_forced_end() {
                    evict_session(&inner, &live, &thread).await;
                }
                // What arrived is kept; the late answer of the agent is dropped.
                buf.push(cancelled());
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

async fn capture_turn_tree(inner: Arc<Inner>, thread: store::Thread) -> Option<String> {
    let parent = inner.snapshot_parent.clone()?;
    let checked_root = lock(&inner.guard)
        .try_check(Path::new(&thread.cwd))
        .ok()?
        .as_path()
        .to_path_buf();
    if checked_root != Path::new(&thread.cwd) {
        return None;
    }
    let project = lock(&inner.store)
        .list_projects()
        .ok()?
        .into_iter()
        .find(|p| p.id == thread.project_id)?;
    let home = inner.home.clone();
    tokio::task::spawn_blocking(move || {
        let project = PathBuf::from(project.path);
        let root = PathBuf::from(&thread.cwd);
        if root != project && thread.worktree_path.as_deref() != Some(thread.cwd.as_str()) {
            return None;
        }
        let owner = FsGuard::new([project.clone()], home.clone());
        let guard = FsGuard::new([project.clone(), root.clone(), parent.clone()], home);
        let (_, common) = owner.git_metadata_dirs(&project, &owner).ok()??;
        let (gitdir, checkout_common) = guard.git_metadata_dirs(&root, &owner).ok()??;
        if common != checkout_common {
            return None;
        }
        let parent = guard.try_check(&parent).ok()?;
        let data = parent.as_path().join("snapshots");
        match std::fs::create_dir(&data) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(_) => return None,
        }
        let paths = hearth_core::snapshot::select(&guard, &data, &root, &gitdir, &common).ok()?;
        hearth_core::snapshot::capture(&guard, &data, &root, &paths).ok()
    })
    .await
    .ok()
    .flatten()
}

fn cancelled() -> AgentItem {
    AgentItem {
        event: AgentEvent::TurnEnd {
            stop_reason: StopReason::Cancelled,
        },
        raw: Value::Null,
    }
}

fn save_session(
    inner: &Inner,
    turn: &store::Turn,
    model: &str,
    external_id: &str,
) -> Result<(), String> {
    lock(&inner.store)
        .observe_turn_session(turn, model, external_id)
        .map_err(|error| format!("Could not save the agent session: {error}"))
}

async fn evict_session(inner: &Inner, live: &Arc<Live>, thread: &ThreadId) {
    let mut sessions = inner.sessions.lock().await;
    if sessions
        .get(thread)
        .is_some_and(|current| Arc::ptr_eq(current, live))
    {
        sessions.remove(thread);
    }
}

async fn fail_stream(
    inner: &Arc<Inner>,
    live: &Arc<Live>,
    thread: &ThreadId,
    turn: &TurnId,
    state: &mut TurnState,
    mut items: Vec<AgentItem>,
    message: String,
) {
    evict_session(inner, live, thread).await;
    items.push(AgentItem {
        event: AgentEvent::Error { message },
        raw: Value::Null,
    });
    flush(inner, live, thread, turn, state, items, true).await;
}

async fn flush(
    inner: &Arc<Inner>,
    live: &Arc<Live>,
    thread: &ThreadId,
    turn: &TurnId,
    state: &mut TurnState,
    items: Vec<AgentItem>,
    last: bool,
) {
    let mut events = coalesce(items);
    let mut auto_answered = Vec::new();
    for e in &mut events {
        if let AgentEvent::ApprovalRequest { request } = &mut e.event {
            if live.remembers_approvals && rule_matches(&lock(&inner.store), thread, request) {
                auto_answered.push(request.clone());
                request.title = format!("{} (allowed for this thread)", request.title);
            } else {
                lock(&live.pending).insert(request.id.clone(), (request.clone(), turn.clone()));
                let _ =
                    lock(&inner.store).update_thread_status(thread, ThreadStatus::AwaitingApproval);
            }
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
    if ended {
        if let Some(end_index) = events.iter().position(is_turn_end) {
            let earlier = events.drain(..end_index).collect();
            let mut payloads = persist(inner, thread, turn, earlier);
            if let Some(first) = payloads.first_mut() {
                first.model = state.model.take();
            }
            for payload in payloads {
                inner.sink.emit(payload);
            }
        }
        let stored = lock(&inner.store).get_thread(thread).ok();
        if let Some(stored) = stored
            && let Some(tree) = capture_turn_tree(inner.clone(), stored).await
        {
            let _ = lock(&inner.store).set_turn_tree_snapshot(turn, &tree);
        }
    }
    let mut payloads = persist(inner, thread, turn, events);
    if let Some(first) = payloads.first_mut() {
        first.model = state.model.take();
    }
    if ended {
        state.ended = true;
        live.process.turn_completed();
        let status = if state.errored {
            ThreadStatus::Error
        } else {
            ThreadStatus::Idle
        };
        let _ = lock(&inner.store).update_thread_status(thread, status);
        // Before the last emit: a client that sends again on `turn.end` must be let in.
        if let Some(ThreadWork::Turn(running)) = lock(&inner.work).remove(thread) {
            running.finished.notify_one();
        }
    }
    for p in payloads {
        inner.sink.emit(p);
    }
    for request in auto_answered {
        let _ = lock(&inner.store).record_approval(turn, &request, ApprovalDecision::AllowOnce);
        let _ = live
            .process
            .answer_approval(&request, ApprovalDecision::AllowOnce)
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
                model: None,
                event: e.event,
            }
        })
        .collect()
}
