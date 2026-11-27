//! The ACP client: process supervision, JSON-RPC framing and the session API.

use crate::mapping::{self, Mapper, PendingPermission};
use futures_core::Stream;
use hearth_protocol::{
    AgentEvent, ApprovalDecision, ApprovalId, ApprovalRequest, Model, UserInput,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::future::poll_fn;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{OwnedMutexGuard, mpsc, oneshot};

const MODEL_PERSIST_KEY: &str = "qwen.session.modelPersistDefault";

/// How to start the agent. The caller decides the environment: spawn clears
/// the inherited one and sets exactly `env`.
#[derive(Debug, Clone)]
pub struct SpawnConfig {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

impl SpawnConfig {
    /// Hearth's fixed Qwen arguments (`--acp --approval-mode default`) and the
    /// four per-spawn privacy switches. The caller adds whatever else the
    /// agent needs to run (a `PATH`, a key variable) to `env`.
    pub fn qwen_defaults(program: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        let env = [
            ("QWEN_USAGE_STATISTICS_ENABLED", "false"),
            ("QWEN_TELEMETRY_ENABLED", "false"),
            ("QWEN_CODE_MODELS_DEV_REFRESH", "off"),
            ("QWEN_CODE_SKIP_UPDATE_CHECK_ONCE", "true"),
        ];
        Self {
            program: program.into(),
            args: ["--acp", "--approval-mode", "default"]
                .map(String::from)
                .to_vec(),
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            cwd: cwd.into(),
        }
    }
}

#[derive(Debug)]
pub enum AdapterError {
    Spawn(std::io::Error),
    Io(std::io::Error),
    /// The agent answered a request with a JSON-RPC error.
    Rpc(String),
    /// The agent process closed its stdout.
    Closed,
    /// No session yet, or an unknown approval, or a missing option.
    State(&'static str),
    /// The agent's reply did not have the expected shape.
    Protocol(&'static str),
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "could not start the agent: {e}"),
            Self::Io(e) => write!(f, "agent i/o failed: {e}"),
            Self::Rpc(m) => write!(f, "agent error: {m}"),
            Self::Closed => write!(f, "the agent process exited"),
            Self::State(m) | Self::Protocol(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for AdapterError {}

/// One normalized event with the agent's raw JSON it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct AdapterItem {
    pub event: AgentEvent,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInfo {
    pub name: String,
    pub version: String,
}

/// What the agent reports about models for a session.
#[derive(Debug, Default)]
pub struct SessionModels {
    /// The model the session is using now (`currentModelId`).
    pub current: Option<String>,
    pub available: Vec<Model>,
}

#[derive(Debug)]
pub struct LoadedSession {
    pub models: SessionModels,
    /// The replayed history, in order.
    pub history: Vec<AdapterItem>,
}

enum Incoming {
    Notification(Value),
    PermissionRequest {
        id: Value,
        params: Value,
        raw: Value,
    },
}

type RpcResult = Result<Value, String>;

struct Shared {
    writer: tokio::sync::Mutex<ChildStdin>,
    pending: Mutex<HashMap<u64, oneshot::Sender<RpcResult>>>,
    next_id: AtomicU64,
    permissions: Mutex<HashMap<ApprovalId, PendingPermission>>,
    session_id: Mutex<Option<String>>,
}

impl Shared {
    async fn write(&self, msg: &Value) -> Result<(), AdapterError> {
        let mut line =
            serde_json::to_vec(msg).map_err(|_| AdapterError::Protocol("unencodable message"))?;
        line.push(b'\n');
        let mut w = self.writer.lock().await;
        w.write_all(&line).await.map_err(AdapterError::Io)?;
        w.flush().await.map_err(AdapterError::Io)
    }

    async fn send_request(
        &self,
        method: &str,
        params: Value,
    ) -> Result<oneshot::Receiver<RpcResult>, AdapterError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let sent = self
            .write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        if let Err(e) = sent {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        Ok(rx)
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, AdapterError> {
        match self.send_request(method, params).await?.await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(m)) => Err(AdapterError::Rpc(m)),
            Err(_) => Err(AdapterError::Closed),
        }
    }

    fn session(&self) -> Result<String, AdapterError> {
        self.session_id
            .lock()
            .unwrap()
            .clone()
            .ok_or(AdapterError::State("no session yet"))
    }

    async fn answer(&self, rpc_id: &Value, outcome: Value) -> Result<(), AdapterError> {
        self.write(&json!({"jsonrpc": "2.0", "id": rpc_id, "result": {"outcome": outcome}}))
            .await
    }
}

async fn read_loop(
    stdout: tokio::process::ChildStdout,
    shared: Arc<Shared>,
    events: mpsc::UnboundedSender<Incoming>,
) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = msg.get("method").and_then(Value::as_str).map(str::to_owned);
        let id = msg.get("id").cloned();
        match (method, id) {
            (Some(m), Some(id)) if m == "session/request_permission" => {
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                if events
                    .send(Incoming::PermissionRequest {
                        id,
                        params,
                        raw: msg,
                    })
                    .is_err()
                {
                    break;
                }
            }
            (Some(_), Some(id)) => {
                // Hearth implements no fs, terminal or private methods.
                let _ = shared
                    .write(&json!({"jsonrpc": "2.0", "id": id,
                        "error": {"code": -32601, "message": "method not found"}}))
                    .await;
            }
            (Some(_), None) => {
                if events.send(Incoming::Notification(msg)).is_err() {
                    break;
                }
            }
            (None, Some(id)) => {
                let Some(n) = id.as_u64() else { continue };
                let tx = shared.pending.lock().unwrap().remove(&n);
                if let Some(tx) = tx {
                    let res = match msg.get("error") {
                        Some(e) => Err(e
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error")
                            .to_string()),
                        None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = tx.send(res);
                }
            }
            (None, None) => {}
        }
    }
    // Closed: wake every waiter.
    shared.pending.lock().unwrap().clear();
}

/// A running `qwen --acp` (or a stand-in) and one session on it.
pub struct QwenProcess {
    shared: Arc<Shared>,
    events: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<Incoming>>>,
    _child: Child,
}

impl QwenProcess {
    pub fn spawn(config: SpawnConfig) -> Result<Self, AdapterError> {
        let mut cmd = Command::new(&config.program);
        cmd.args(&config.args)
            .env_clear()
            .envs(config.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&config.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(AdapterError::Spawn)?;
        let stdin = child.stdin.take().ok_or(AdapterError::State("no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or(AdapterError::State("no stdout"))?;
        let shared = Arc::new(Shared {
            writer: tokio::sync::Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            permissions: Mutex::new(HashMap::new()),
            session_id: Mutex::new(None),
        });
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(read_loop(stdout, shared.clone(), tx));
        Ok(Self {
            shared,
            events: Arc::new(tokio::sync::Mutex::new(rx)),
            _child: child,
        })
    }

    /// Protocol version 1; declares no `fs` and no `terminal` capability.
    pub async fn initialize(&self) -> Result<AgentInfo, AdapterError> {
        let res = self
            .shared
            .request(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {
                        "fs": {"readTextFile": false, "writeTextFile": false},
                        "terminal": false
                    },
                    "clientInfo": {"name": "hearth", "version": env!("CARGO_PKG_VERSION")}
                }),
            )
            .await?;
        let s = |k| {
            res.pointer(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        Ok(AgentInfo {
            name: s("/agentInfo/name"),
            version: s("/agentInfo/version"),
        })
    }

    pub async fn new_session(&self, cwd: &Path) -> Result<(String, SessionModels), AdapterError> {
        let res = self
            .shared
            .request("session/new", json!({"cwd": cwd, "mcpServers": []}))
            .await?;
        let id = res
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or(AdapterError::Protocol("session/new returned no sessionId"))?
            .to_string();
        *self.shared.session_id.lock().unwrap() = Some(id.clone());
        Ok((id, models(&res)))
    }

    /// Resume a session; the agent replays its history before answering.
    pub async fn load_session(&self, id: &str, cwd: &Path) -> Result<LoadedSession, AdapterError> {
        *self.shared.session_id.lock().unwrap() = Some(id.to_string());
        let mut events = self.events.clone().lock_owned().await;
        let res = self
            .shared
            .request(
                "session/load",
                json!({"sessionId": id, "cwd": cwd, "mcpServers": []}),
            )
            .await?;
        // The reader queues every replayed update before it routes the response.
        let mut mapper = Mapper::default();
        let mut history = Vec::new();
        while let Ok(i) = events.try_recv() {
            history.extend(handle(&mut mapper, &self.shared, i));
        }
        Ok(LoadedSession {
            models: models(&res),
            history,
        })
    }

    /// Send one prompt. The stream ends after the `turn.end` (or `error`) item.
    pub fn prompt(&self, input: UserInput) -> AdapterStream {
        let (tx, rx) = mpsc::channel(1024);
        let shared = self.shared.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let mut events: OwnedMutexGuard<_> = events.lock_owned().await;
            pump(&shared, &mut events, input, &tx).await;
        });
        AdapterStream { rx }
    }

    /// Answer an approval. Allow and allow-for-thread both pick the agent's
    /// one-shot allow option, deny its one-shot reject; never an "always".
    pub async fn answer_permission(
        &self,
        request: &ApprovalRequest,
        decision: ApprovalDecision,
    ) -> Result<(), AdapterError> {
        let pending = self
            .shared
            .permissions
            .lock()
            .unwrap()
            .remove(&request.id)
            .ok_or(AdapterError::State("unknown or already answered approval"))?;
        let option = match decision {
            ApprovalDecision::AllowOnce | ApprovalDecision::AllowForThread => pending.allow_once,
            ApprovalDecision::Deny => pending.reject_once,
        }
        .ok_or(AdapterError::State(
            "the agent offered no one-shot option for this answer",
        ))?;
        self.shared
            .answer(
                &pending.rpc_id,
                json!({"outcome": "selected", "optionId": option}),
            )
            .await
    }

    /// Cancel the running turn and answer any open approval as cancelled.
    pub async fn cancel(&self) -> Result<(), AdapterError> {
        let open: Vec<PendingPermission> = self
            .shared
            .permissions
            .lock()
            .unwrap()
            .drain()
            .map(|(_, p)| p)
            .collect();
        for p in open {
            self.shared
                .answer(&p.rpc_id, json!({"outcome": "cancelled"}))
                .await?;
        }
        let session = self.shared.session()?;
        self.shared
            .write(&json!({"jsonrpc": "2.0", "method": "session/cancel",
                "params": {"sessionId": session}}))
            .await
    }

    /// Switch model for this session only; the agent must not persist it.
    pub async fn set_model(&self, model: &str) -> Result<(), AdapterError> {
        let session = self.shared.session()?;
        // The response carries a masked key in `_meta.qwenModelSwitch`; it is
        // dropped here and never returned.
        self.shared
            .request(
                "session/set_model",
                json!({"sessionId": session, "modelId": model,
                    "_meta": {MODEL_PERSIST_KEY: false}}),
            )
            .await
            .map(|_| ())
    }

    /// A raw request, for the replay stub's debug methods in tests.
    #[doc(hidden)]
    pub async fn debug_request(&self, method: &str, params: Value) -> Result<Value, AdapterError> {
        self.shared.request(method, params).await
    }
}

fn models(res: &Value) -> SessionModels {
    let current = res
        .pointer("/models/currentModelId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let available = available_models(res);
    SessionModels { current, available }
}

fn available_models(res: &Value) -> Vec<Model> {
    res.pointer("/models/availableModels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m.get("modelId")?.as_str()?.to_string();
            let label = m
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(&id)
                .to_string();
            Some(Model { id, label })
        })
        .collect()
}

fn handle(mapper: &mut Mapper, shared: &Shared, incoming: Incoming) -> Vec<AdapterItem> {
    match incoming {
        Incoming::Notification(raw) => {
            if raw.get("method").and_then(Value::as_str) != Some("session/update") {
                return vec![];
            }
            let events = mapper.update(raw.get("params").unwrap_or(&Value::Null));
            events
                .into_iter()
                .map(|event| AdapterItem {
                    event,
                    raw: raw.clone(),
                })
                .collect()
        }
        Incoming::PermissionRequest { id, params, raw } => {
            let approval_id = ApprovalId(uuid::Uuid::now_v7().to_string());
            let (request, pending) = mapping::permission(&params, approval_id.clone(), id);
            shared
                .permissions
                .lock()
                .unwrap()
                .insert(approval_id, pending);
            vec![AdapterItem {
                event: AgentEvent::ApprovalRequest { request },
                raw,
            }]
        }
    }
}

async fn pump(
    shared: &Arc<Shared>,
    events: &mut mpsc::UnboundedReceiver<Incoming>,
    input: UserInput,
    tx: &mpsc::Sender<AdapterItem>,
) {
    let fail = |message: String| AdapterItem {
        raw: json!({"error": message}),
        event: AgentEvent::Error { message },
    };
    let UserInput::Text { text } = input;
    let started = async {
        let session = shared.session()?;
        shared
            .send_request(
                "session/prompt",
                json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]}),
            )
            .await
    }
    .await;
    let mut done = match started {
        Ok(rx) => rx,
        Err(e) => {
            let _ = tx.send(fail(e.to_string())).await;
            return;
        }
    };
    let mut mapper = Mapper::default();
    loop {
        tokio::select! {
            incoming = events.recv() => match incoming {
                Some(i) => {
                    for item in handle(&mut mapper, shared, i) {
                        if tx.send(item).await.is_err() {
                            return;
                        }
                    }
                }
                None => {
                    let _ = tx.send(fail(AdapterError::Closed.to_string())).await;
                    return;
                }
            },
            res = &mut done => {
                // Everything the agent sent before its answer is queued already.
                while let Ok(i) = events.try_recv() {
                    for item in handle(&mut mapper, shared, i) {
                        if tx.send(item).await.is_err() {
                            return;
                        }
                    }
                }
                let item = match res {
                    Ok(Ok(result)) => {
                        let reason = result.get("stopReason").and_then(Value::as_str).unwrap_or("end_turn");
                        AdapterItem {
                            event: AgentEvent::TurnEnd { stop_reason: mapping::stop_reason(reason) },
                            raw: result,
                        }
                    }
                    Ok(Err(m)) => fail(m),
                    Err(_) => fail(AdapterError::Closed.to_string()),
                };
                let _ = tx.send(item).await;
                return;
            }
        }
    }
}

/// The events of one turn.
pub struct AdapterStream {
    rx: mpsc::Receiver<AdapterItem>,
}

impl AdapterStream {
    pub async fn next(&mut self) -> Option<AdapterItem> {
        poll_fn(|cx| self.poll_next(cx)).await
    }

    pub fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<AdapterItem>> {
        self.rx.poll_recv(cx)
    }
}

impl Stream for AdapterStream {
    type Item = AdapterItem;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<AdapterItem>> {
        self.rx.poll_recv(cx)
    }
}
