//! Direct stream-json client for the installed Claude CLI.

mod wire;

use hearth_protocol::{AgentEvent, ApprovalDecision, ApprovalId};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    ffi::OsString,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex as AsyncMutex, mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};
use wire::{ControlResponse, Envelope};

/// Versions verified with a signed-in real run (event, approval, resume, cancel).
const SUPPORTED_VERSIONS: &[&str] = &["2.1.328", "2.1.332"];
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
const TURN_BUFFER: usize = 1024;

#[derive(Clone)]
pub struct SpawnConfig {
    pub executable: PathBuf,
    pub cwd: PathBuf,
    pub env: Vec<(OsString, OsString)>,
    pub resume: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    pub value: String,
    pub label: String,
    pub resolved_id: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Item {
    Session {
        id: String,
        model: String,
        analytics_disabled: Option<bool>,
    },
    Event {
        event: AgentEvent,
        raw: Value,
    },
    Observation {
        raw: Value,
    },
}

/// Errors deliberately exclude raw child output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Launch,
    Io,
    InvalidWire,
    UnsupportedVersion(String),
    Closed,
    Timeout,
    Busy,
    NoTurn,
    UnknownApproval,
    RejectedControl,
    QueueFull,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedVersion(version) => {
                write!(f, "Unsupported Claude Code version {version}")
            }
            _ => write!(f, "Claude adapter: {self:?}"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Default)]
struct State {
    closed: Option<Error>,
    controls: HashMap<String, oneshot::Sender<Result<ControlResponse, Error>>>,
    turn: Option<RunningTurn>,
}

struct RunningTurn {
    id: u64,
    output: mpsc::Sender<Item>,
    failure: Arc<Mutex<Option<Error>>>,
    decoder: wire::TurnDecoder,
    approvals: HashMap<String, Option<Value>>,
    interrupt_requested: bool,
}

struct WriteRequest {
    line: Vec<u8>,
    completed: oneshot::Sender<Result<(), Error>>,
}

struct Transport {
    cwd: PathBuf,
    writes: mpsc::Sender<WriteRequest>,
    state: Mutex<State>,
    next_id: AtomicU64,
}

impl Transport {
    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn close(&self, failure: Error) {
        let mut state = self.state.lock().unwrap();
        if state.closed.is_some() {
            return;
        }
        state.closed = Some(failure.clone());
        if let Some(turn) = state.turn.take() {
            *turn.failure.lock().unwrap() = Some(failure.clone());
        }
        for (_, pending) in state.controls.drain() {
            let _ = pending.send(Err(failure.clone()));
        }
    }

    async fn write(&self, message: Value) -> Result<(), Error> {
        let mut line = serde_json::to_vec(&message).map_err(|_| Error::InvalidWire)?;
        line.push(b'\n');
        let (completed, result) = oneshot::channel();
        let outcome = timeout(CONTROL_TIMEOUT, async {
            if let Some(error) = self.state.lock().unwrap().closed.clone() {
                return Err(error);
            }
            self.writes
                .send(WriteRequest { line, completed })
                .await
                .map_err(|_| Error::Closed)?;
            result.await.map_err(|_| Error::Closed)?
        })
        .await
        .unwrap_or(Err(Error::Timeout));
        if let Err(error) = &outcome {
            self.close(error.clone());
        }
        outcome
    }

    async fn request(&self, request: Value) -> Result<ControlResponse, Error> {
        let id = self.next_id().to_string();
        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.state.lock().unwrap();
            if let Some(error) = &state.closed {
                return Err(error.clone());
            }
            state.controls.insert(id.clone(), tx);
        }
        let _pending = PendingControl(self, &id);
        self.write(json!({"type":"control_request", "request_id":id, "request":request}))
            .await?;
        timeout(CONTROL_TIMEOUT, rx)
            .await
            .map_err(|_| Error::Timeout)?
            .map_err(|_| Error::Closed)?
    }

    fn dispatch(&self, envelope: Envelope) -> Result<(), Error> {
        let mut state = self.state.lock().unwrap();
        if let Envelope::ControlResponse { response } = envelope {
            let id = response.request_id().to_owned();
            if let Some(pending) = state.controls.remove(&id) {
                let _ = pending.send(if response.succeeded() {
                    Ok(response)
                } else {
                    Err(Error::RejectedControl)
                });
            }
            return Ok(());
        }
        let Some(turn) = &mut state.turn else {
            return Ok(());
        };
        let ended = matches!(envelope, Envelope::Result { .. });
        let items = turn.decoder.decode(
            envelope,
            &self.cwd,
            &mut turn.approvals,
            turn.interrupt_requested,
        );
        for item in items {
            if matches!(
                turn.output.try_send(item),
                Err(mpsc::error::TrySendError::Full(_))
            ) {
                return Err(Error::QueueFull);
            }
        }
        if ended {
            state.turn = None;
        }
        Ok(())
    }
}

struct PendingControl<'a>(&'a Transport, &'a str);

impl Drop for PendingControl<'_> {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().controls.remove(self.1);
    }
}

pub struct TurnStream {
    items: mpsc::Receiver<Item>,
    failure: Arc<Mutex<Option<Error>>>,
}

impl TurnStream {
    pub async fn next_item(&mut self) -> Option<Result<Item, Error>> {
        std::future::poll_fn(|cx| self.poll_next(cx)).await
    }

    pub fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Item, Error>>> {
        self.items.poll_recv(cx).map(|item| {
            item.map(Ok)
                .or_else(|| self.failure.lock().unwrap().take().map(Err))
        })
    }
}

pub struct ClaudeProcess {
    transport: Arc<Transport>,
    submit: AsyncMutex<()>,
    models: Vec<ModelChoice>,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
    _child: Child,
}

impl ClaudeProcess {
    pub async fn detect(config: &SpawnConfig) -> Result<String, Error> {
        let output = timeout(CONTROL_TIMEOUT, command(config).arg("--version").output())
            .await
            .map_err(|_| Error::Timeout)?
            .map_err(|_| Error::Launch)?;
        let version = std::str::from_utf8(&output.stdout)
            .ok()
            .and_then(|text| text.split_whitespace().next())
            .filter(|text| {
                text.len() <= 24
                    && text.split('.').count() == 3
                    && text
                        .split('.')
                        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
            })
            .ok_or(Error::Launch)?;
        if !output.status.success() {
            return Err(Error::Launch);
        }
        if !SUPPORTED_VERSIONS.contains(&version) {
            return Err(Error::UnsupportedVersion(version.to_owned()));
        }
        Ok(version.to_owned())
    }

    pub async fn spawn(config: &SpawnConfig) -> Result<Self, Error> {
        Self::detect(config).await?;
        let mut launch = command(config);
        launch.args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--permission-prompt-tool",
            "stdio",
        ]);
        if let Some(id) = &config.resume {
            launch.arg("--resume").arg(id);
        }
        let mut child = launch.spawn().map_err(|_| Error::Launch)?;
        let writer = child.stdin.take().ok_or(Error::Launch)?;
        let stdout = child.stdout.take().ok_or(Error::Launch)?;
        let (writes, requests) = mpsc::channel(16);
        let transport = Arc::new(Transport {
            cwd: config.cwd.clone(),
            writes,
            state: Mutex::new(State::default()),
            next_id: AtomicU64::new(1),
        });
        let reader = tokio::spawn(read_stdout(stdout, transport.clone()));
        let mut client = Self {
            transport: transport.clone(),
            submit: AsyncMutex::new(()),
            models: Vec::new(),
            reader,
            writer: tokio::spawn(write_stdin(writer, requests, transport.clone())),
            _child: child,
        };
        client.models = client
            .transport
            .request(json!({"subtype":"initialize"}))
            .await?
            .models()
            .ok_or(Error::InvalidWire)?;
        Ok(client)
    }

    pub fn models(&self) -> &[ModelChoice] {
        &self.models
    }

    pub async fn prompt(&self, text: &str) -> Result<TurnStream, Error> {
        let _submit = self.submit.lock().await;
        let (output, items) = mpsc::channel(TURN_BUFFER);
        let failure = Arc::new(Mutex::new(None));
        {
            let mut state = self.transport.state.lock().unwrap();
            if let Some(error) = &state.closed {
                return Err(error.clone());
            }
            if state.turn.is_some() {
                return Err(Error::Busy);
            }
            state.turn = Some(RunningTurn {
                id: self.transport.next_id(),
                output,
                failure: failure.clone(),
                decoder: wire::TurnDecoder::default(),
                approvals: HashMap::new(),
                interrupt_requested: false,
            });
        }
        self.transport
            .write(json!({"type":"user",
            "message":{"role":"user", "content":text}, "parent_tool_use_id":null}))
            .await?;
        Ok(TurnStream { items, failure })
    }

    pub async fn interrupt(&self) -> Result<(), Error> {
        let _submit = self.submit.lock().await;
        let turn_id = {
            let mut state = self.transport.state.lock().unwrap();
            let turn = state.turn.as_mut().ok_or(Error::NoTurn)?;
            if turn.interrupt_requested {
                return Ok(());
            }
            turn.interrupt_requested = true;
            turn.id
        };
        let result = self
            .transport
            .request(json!({"subtype":"interrupt"}))
            .await
            .map(|_| ());
        if result.is_err() {
            let mut state = self.transport.state.lock().unwrap();
            if let Some(turn) = &mut state.turn
                && turn.id == turn_id
            {
                turn.interrupt_requested = false;
            }
        }
        result
    }

    pub async fn set_model(&self, model: &str) -> Result<Option<String>, Error> {
        let _submit = self.submit.lock().await;
        if self.transport.state.lock().unwrap().turn.is_some() {
            return Err(Error::Busy);
        }
        self.transport
            .request(json!({"subtype":"set_model", "model":model}))
            .await
            .map(ControlResponse::model)
    }

    pub async fn answer(&self, id: &ApprovalId, decision: ApprovalDecision) -> Result<(), Error> {
        let input = self
            .transport
            .state
            .lock()
            .unwrap()
            .turn
            .as_mut()
            .and_then(|turn| turn.approvals.get_mut(&id.0))
            .and_then(Option::take)
            .ok_or(Error::UnknownApproval)?;
        let reply = match decision {
            ApprovalDecision::Deny => json!({"behavior":"deny", "message":"Denied by user"}),
            ApprovalDecision::AllowOnce | ApprovalDecision::AllowForThread => {
                json!({"behavior":"allow", "updatedInput":input})
            }
        };
        self.transport
            .write(json!({"type":"control_response", "response":{
                "subtype":"success", "request_id":id.0, "response":reply
            }}))
            .await
    }
}

impl Drop for ClaudeProcess {
    fn drop(&mut self) {
        self.transport.close(Error::Closed);
        self.reader.abort();
        self.writer.abort();
    }
}

fn command(config: &SpawnConfig) -> Command {
    let mut command = Command::new(&config.executable);
    command
        .current_dir(&config.cwd)
        .env_clear()
        .envs(config.env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}

async fn read_stdout(stdout: ChildStdout, transport: Arc<Transport>) {
    let mut lines = BufReader::new(stdout).lines();
    let failure = loop {
        match lines.next_line().await {
            Ok(Some(line)) if line.trim().is_empty() => continue,
            Ok(Some(line)) => {
                let Ok(envelope) = serde_json::from_str(&line) else {
                    break Error::InvalidWire;
                };
                if let Err(error) = transport.dispatch(envelope) {
                    break error;
                }
            }
            Ok(None) => break Error::Closed,
            Err(_) => break Error::Io,
        }
    };
    transport.close(failure);
}

async fn write_stdin(
    mut stdin: ChildStdin,
    mut requests: mpsc::Receiver<WriteRequest>,
    transport: Arc<Transport>,
) {
    while let Some(WriteRequest { line, completed }) = requests.recv().await {
        let outcome = timeout(CONTROL_TIMEOUT, async {
            stdin.write_all(&line).await?;
            stdin.flush().await
        })
        .await
        .map_err(|_| Error::Timeout)
        .and_then(|result| result.map_err(|_| Error::Io));
        if let Err(error) = &outcome {
            transport.close(error.clone());
        }
        let failed = outcome.is_err();
        let _ = completed.send(outcome);
        if failed {
            break;
        }
    }
}
