use super::login_shell_output;
use super::{
    AgentAdapter, AgentFuture, AgentItem, AgentSession, AgentStream, OpenSession, StreamItem,
};
use crate::api::AgentStatus;
use hearth_adapter_claude::{ClaudeProcess, Error, Item, SpawnConfig, TurnStream};
use hearth_protocol::{ApprovalDecision, ApprovalRequest, UserInput};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::OnceCell;

const HOST_ENV: &[&str] = &[
    "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "LC_CTYPE", "TERM", "TMPDIR",
];

pub struct ClaudeAdapter {
    home: PathBuf,
    shell: Option<PathBuf>,
    program_override: Option<PathBuf>,
    paths: OnceCell<Vec<PathBuf>>,
}

impl ClaudeAdapter {
    pub fn new(home: PathBuf, shell: Option<PathBuf>, program_override: Option<PathBuf>) -> Self {
        Self {
            home,
            shell,
            program_override,
            paths: OnceCell::new(),
        }
    }

    async fn paths(&self) -> &[PathBuf] {
        self.paths
            .get_or_init(|| async {
                let login = match self.shell.clone() {
                    Some(shell) => {
                        let home = self.home.clone();
                        tokio::task::spawn_blocking(move || {
                            login_shell_output(&shell, &home, "printf '\\n%s' \"$PATH\"")
                        })
                        .await
                        .ok()
                        .flatten()
                    }
                    None => None,
                };
                let inherited_path = std::env::var_os("PATH");
                let login_path = login
                    .as_deref()
                    .and_then(|text| text.rsplit('\n').next())
                    .map(OsStr::new);
                let mut paths: Vec<_> = [login_path, inherited_path.as_deref()]
                    .into_iter()
                    .flatten()
                    .flat_map(std::env::split_paths)
                    .filter(|path| path.is_absolute())
                    .collect();
                paths.extend([
                    self.home.join(".local/bin"),
                    PathBuf::from("/opt/homebrew/bin"),
                    PathBuf::from("/usr/local/bin"),
                    PathBuf::from("/usr/bin"),
                    PathBuf::from("/bin"),
                ]);
                paths
            })
            .await
    }

    async fn config(&self, cwd: &Path, resume_id: Option<&str>) -> Option<SpawnConfig> {
        let paths = self.paths().await;
        let executable = match &self.program_override {
            Some(program) => program.clone(),
            None => paths
                .iter()
                .map(|dir| dir.join("claude"))
                .find(|path| path.is_file())?,
        };

        Some(SpawnConfig {
            executable,
            cwd: cwd.to_owned(),
            env: spawn_env(
                &self.home,
                std::env::join_paths(paths).ok()?.as_os_str(),
                |name| std::env::var_os(name),
            ),
            resume: resume_id.map(str::to_owned),
        })
    }
}

fn spawn_env(
    home: &Path,
    path: &OsStr,
    host: impl Fn(&str) -> Option<OsString>,
) -> Vec<(OsString, OsString)> {
    let mut env: Vec<_> = HOST_ENV
        .iter()
        .filter_map(|&name| host(name).map(|value| (name.into(), value)))
        .collect();
    env.extend([
        ("HOME".into(), home.as_os_str().to_owned()),
        ("PATH".into(), path.to_owned()),
        ("DISABLE_TELEMETRY".into(), "1".into()),
        ("DO_NOT_TRACK".into(), "1".into()),
        (
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(),
            "1".into(),
        ),
    ]);
    env
}

impl AgentAdapter for ClaudeAdapter {
    fn availability(&self) -> AgentFuture<'_, AgentStatus> {
        Box::pin(async move {
            let Some(config) = self.config(&self.home, None).await else {
                return AgentStatus::Missing;
            };
            match ClaudeProcess::detect(&config).await {
                Ok(_) => AgentStatus::Ready,
                Err(Error::UnsupportedVersion(version)) => AgentStatus::Unsupported { version },
                Err(_) => AgentStatus::Missing,
            }
        })
    }

    fn open_session<'a>(
        &'a self,
        cwd: &'a Path,
        resume_id: Option<&'a str>,
        _passthrough: &'a [String],
    ) -> AgentFuture<'a, Result<OpenSession, String>> {
        Box::pin(async move {
            let config = self.config(cwd, resume_id).await.ok_or(
                "Claude Code was not found. Install Claude Code and sign in from your terminal.",
            )?;
            let process = ClaudeProcess::spawn(&config)
                .await
                .map_err(|error| error.to_string())?;
            Ok(OpenSession {
                session: Arc::new(ClaudeSession(process)),
                external_id: None,
                model: None,
            })
        })
    }
}

struct ClaudeSession(ClaudeProcess);

impl AgentSession for ClaudeSession {
    fn prompt(&self, input: UserInput) -> AgentFuture<'_, Result<Box<dyn AgentStream>, String>> {
        Box::pin(async move {
            let UserInput::Text { text } = input;
            let source = self
                .0
                .prompt(&text)
                .await
                .map_err(|error| error.to_string())?;
            let stream: Box<dyn AgentStream> = Box::new(ClaudeStream(source));
            Ok(stream)
        })
    }

    fn reopen_after_forced_end(&self) -> bool {
        true
    }

    fn turn_completed(&self) {}

    fn answer_approval<'a>(
        &'a self,
        request: &'a ApprovalRequest,
        decision: ApprovalDecision,
    ) -> AgentFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.0
                .answer(&request.id, decision)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn cancel(&self) -> AgentFuture<'_, Result<(), String>> {
        Box::pin(async move { self.0.interrupt().await.map_err(|error| error.to_string()) })
    }
}

struct ClaudeStream(TurnStream);

impl AgentStream for ClaudeStream {
    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<StreamItem>> {
        loop {
            return match std::task::ready!(self.0.poll_next(cx)) {
                Some(Ok(Item::Session { id, model, .. })) => {
                    Poll::Ready(Some(StreamItem::Session {
                        external_id: id,
                        model,
                    }))
                }
                Some(Ok(Item::Event { event, raw })) => {
                    Poll::Ready(Some(StreamItem::Event(AgentItem { event, raw })))
                }
                Some(Ok(Item::Observation { .. })) => continue,
                Some(Err(error)) => Poll::Ready(Some(StreamItem::Failed(error.to_string()))),
                None => Poll::Ready(None),
            };
        }
    }
}
