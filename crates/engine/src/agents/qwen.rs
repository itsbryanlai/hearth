//! Finding Qwen Code and building its spawn environment. No credential or
//! config file is read here: the environment is built from a fixed
//! allowlist plus the variable names the user chose, with values taken from
//! the login shell. No value is ever logged, stored or emitted.

use super::{AgentAdapter, AgentFuture, AgentItem, AgentSession, AgentStream, OpenSession};
use hearth_adapter_qwen::QwenProcess;
use hearth_adapter_qwen::SpawnConfig;
use hearth_protocol::{AgentEvent, ApprovalDecision, ApprovalRequest, UserInput};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

/// Variables passed on from Hearth's own environment (only those present).
pub const ENV_ALLOWLIST: [&str; 9] = [
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "LC_CTYPE", "TMPDIR",
];
/// `TERM` is allowed too; kept apart so the list above reads as the base set.
const ENV_ALLOWLIST_EXTRA: [&str; 1] = ["TERM"];

/// The variables Qwen Code's own `openai` auth method names in `initialize`.
pub const DEFAULT_PASSTHROUGH: [&str; 3] = ["OPENAI_API_KEY", "OPENAI_BASE_URL", "OPENAI_MODEL"];

/// The setting that holds the pass-through names.
pub const PASSTHROUGH_SETTING: &str = "agents.qwen-code.envPassthrough";

pub const NOT_FOUND: &str = "Qwen Code not found: install it, or run `qwen` once in your terminal";

/// The agent's environment. The one place the policy lives.
///
/// - `host(name)`: Hearth's own environment; only allowlisted names are asked.
/// - `login(name)`: the value of a variable in the user's login shell. Used
///   for `PATH` (a GUI app does not inherit the shell's) and for every
///   pass-through name. A pass-through value never comes from `host`.
/// - `passthrough`: names the user chose. A name that is unset is skipped;
///   a name that is not a plain identifier, or that would override one of
///   the privacy switches, is ignored.
pub fn spawn_env(
    host: &dyn Fn(&str) -> Option<String>,
    login: &dyn Fn(&str) -> Option<String>,
    passthrough: &[String],
) -> Vec<(String, String)> {
    let privacy = SpawnConfig::qwen_defaults("", "").env;
    let mut env = BTreeMap::new();
    for name in ENV_ALLOWLIST.iter().chain(ENV_ALLOWLIST_EXTRA.iter()) {
        if let Some(v) = host(name) {
            env.insert(name.to_string(), v);
        }
    }
    if let Some(path) = login("PATH").filter(|p| !p.is_empty()) {
        env.insert("PATH".into(), path);
    }
    for name in passthrough {
        if !is_identifier(name) || privacy.iter().any(|(k, _)| k == name) {
            continue;
        }
        if let Some(v) = login(name).filter(|v| !v.is_empty()) {
            env.insert(name.clone(), v);
        }
    }
    env.extend(privacy);
    env.into_iter().collect()
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Runs a command with a time limit; stdout on success, `None` otherwise.
fn run_limited(mut cmd: Command, limit: Duration) -> Option<String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout.read_to_string(&mut out);
        out
    });
    let start = Instant::now();
    let ok = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if start.elapsed() < limit => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    let out = reader.join().ok()?;
    ok.then_some(out)
}

/// What the login shell told us, read once.
struct Detected {
    program: PathBuf,
    path: Option<String>,
}

/// Finds `qwen` and builds its spawn config.
pub struct QwenLauncher {
    program_override: Option<PathBuf>,
    home: PathBuf,
    shell: PathBuf,
    detected: OnceLock<Result<Detected, String>>,
}

impl QwenLauncher {
    /// `program_override`: the dev hook (`HEARTH_QWEN_PROGRAM` in Hearth's own
    /// environment, read by the app, not a user setting). Set, it replaces
    /// detection, so a replay stub can stand in for `qwen`.
    pub fn new(home: PathBuf, shell: Option<PathBuf>, program_override: Option<PathBuf>) -> Self {
        Self {
            program_override,
            home,
            shell: shell.unwrap_or_else(|| PathBuf::from("/bin/sh")),
            detected: OnceLock::new(),
        }
    }

    /// Reads the login shell's PATH and finds `qwen` now (otherwise on first
    /// spawn). Blocking; safe to call from a background thread at startup.
    pub fn prewarm(&self) {
        let _ = self.detect();
    }

    fn shell_output(&self, script: &str) -> Option<String> {
        let mut cmd = Command::new(&self.shell);
        cmd.args(["-lc", script])
            .env_clear()
            .env("HOME", &self.home)
            .env("SHELL", &self.shell);
        for name in ["USER", "LOGNAME"] {
            if let Ok(v) = std::env::var(name) {
                cmd.env(name, v);
            }
        }
        run_limited(cmd, Duration::from_secs(10))
    }

    /// One variable's value in the login shell. A login shell may print noise
    /// from its rc files first; the value is the last line.
    fn login_value(&self, name: &str) -> Option<String> {
        if !is_identifier(name) {
            return None;
        }
        let out = self.shell_output(&format!("printf '\\n%s' \"${name}\""))?;
        out.rsplit('\n').next().map(str::to_owned)
    }

    fn detect(&self) -> &Result<Detected, String> {
        self.detected.get_or_init(|| {
            let path = self.login_value("PATH").filter(|p| !p.is_empty());
            let from_shell = self
                .shell_output("command -v qwen")
                .and_then(|o| o.lines().last().map(|l| PathBuf::from(l.trim())))
                .filter(|p| p.is_absolute());
            let candidates = from_shell.into_iter().chain([
                self.home.join(".local/bin/qwen"),
                PathBuf::from("/opt/homebrew/bin/qwen"),
                PathBuf::from("/usr/local/bin/qwen"),
            ]);
            for program in candidates {
                if program.is_file() && version_works(&program, path.as_deref(), &self.home) {
                    return Ok(Detected { program, path });
                }
            }
            Err(NOT_FOUND.to_string())
        })
    }
}

fn version_works(program: &Path, path: Option<&str>, home: &Path) -> bool {
    let mut cmd = Command::new(program);
    cmd.arg("--version").env_clear().env("HOME", home);
    if let Some(p) = path {
        cmd.env("PATH", p);
    }
    run_limited(cmd, Duration::from_secs(10)).is_some()
}

impl AgentLauncher for QwenLauncher {
    fn spawn_config(&self, cwd: &Path, passthrough: &[String]) -> Result<SpawnConfig, String> {
        let program = match &self.program_override {
            Some(p) => p.clone(),
            None => self
                .detect()
                .as_ref()
                .map_err(|e| e.clone())?
                .program
                .clone(),
        };
        let login_path = self.detect().as_ref().ok().and_then(|d| d.path.clone());
        let host = |name: &str| std::env::var(name).ok();
        let login = |name: &str| {
            if name == "PATH" {
                login_path.clone().or_else(|| self.login_value(name))
            } else {
                self.login_value(name)
            }
        };
        let mut config = SpawnConfig::qwen_defaults(program, cwd);
        config.env = spawn_env(&host, &login, passthrough);
        Ok(config)
    }
}

/// Supplies Qwen's spawn policy on a blocking thread.
pub trait AgentLauncher: Send + Sync {
    fn spawn_config(&self, cwd: &Path, passthrough: &[String]) -> Result<SpawnConfig, String>;
}

/// Opens Qwen sessions using the supplied launch policy.
pub struct QwenAdapter(pub Arc<dyn AgentLauncher>);

impl AgentAdapter for QwenAdapter {
    fn open_session<'a>(
        &'a self,
        cwd: &'a Path,
        resume_id: Option<&'a str>,
        passthrough: &'a [String],
    ) -> AgentFuture<'a, Result<OpenSession, String>> {
        Box::pin(async move {
            let launcher = self.0.clone();
            let spawn_cwd = cwd.to_path_buf();
            let names = passthrough.to_vec();
            let config =
                tokio::task::spawn_blocking(move || launcher.spawn_config(&spawn_cwd, &names))
                    .await
                    .map_err(|e| e.to_string())??;
            let process = QwenProcess::spawn(config).map_err(|e| e.to_string())?;
            process
                .initialize()
                .await
                .map_err(|e| with_auth_hint(&e.to_string()))?;
            let resumed = match resume_id {
                Some(id) => process
                    .load_session(id, cwd)
                    .await
                    .ok()
                    .map(|s| (id.to_owned(), s.models)),
                None => None,
            };
            let had_session = resumed.is_some();
            let (external_id, models) = match resumed {
                Some(value) => value,
                None => process
                    .new_session(cwd)
                    .await
                    .map_err(|e| with_auth_hint(&e.to_string()))?,
            };
            let model = models
                .current
                .or_else(|| models.available.first().map(|m| m.id.clone()))
                .unwrap_or_default();
            Ok(OpenSession {
                session: Arc::new(QwenSession {
                    process,
                    turn_done: Arc::new(AtomicBool::new(had_session)),
                }),
                external_id: Some(external_id),
                model: Some(model),
            })
        })
    }
}

struct QwenSession {
    process: QwenProcess,
    turn_done: Arc<AtomicBool>,
}
impl AgentSession for QwenSession {
    fn prompt(&self, input: UserInput) -> Box<dyn AgentStream> {
        Box::new(QwenStream {
            source: self.process.prompt(input),
            done: self.turn_done.clone(),
        })
    }
    fn turn_completed(&self) {
        self.turn_done.store(true, Ordering::Relaxed);
    }
    fn answer_approval<'a>(
        &'a self,
        request: &'a ApprovalRequest,
        decision: ApprovalDecision,
    ) -> AgentFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.process
                .answer_permission(request, decision)
                .await
                .map_err(|e| e.to_string())
        })
    }
    fn cancel(&self) -> AgentFuture<'_, Result<(), String>> {
        Box::pin(async move { self.process.cancel().await.map_err(|e| e.to_string()) })
    }
}

struct QwenStream {
    source: hearth_adapter_qwen::AdapterStream,
    done: Arc<AtomicBool>,
}
impl AgentStream for QwenStream {
    fn poll_next(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<AgentItem>> {
        self.source.poll_next(cx).map(|item| {
            item.map(|mut item| {
                if let AgentEvent::Error { message } = &mut item.event
                    && !self.done.load(Ordering::Relaxed)
                {
                    *message = with_auth_hint(message);
                }
                AgentItem {
                    event: item.event,
                    raw: item.raw,
                }
            })
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    // Fails if the environment leaks a variable outside the allowlist, takes a
    // pass-through value from Hearth's own environment, lets a pass-through
    // name override a privacy switch, or drops the shell's PATH.
    #[test]
    fn env_policy_keeps_allowlist_passthrough_and_privacy_only() {
        let host = lookup(&[
            ("PATH", "/usr/bin"),
            ("HOME", "/Users/x"),
            ("TERM", "xterm"),
            ("AWS_SECRET_ACCESS_KEY", "s1"),
            ("ANTHROPIC_API_KEY", "s2"),
            ("OPENAI_API_KEY", "from-host"),
        ]);
        let login = lookup(&[
            ("PATH", "/opt/homebrew/bin:/usr/bin"),
            ("OPENAI_API_KEY", "from-shell"),
            ("OPENAI_BASE_URL", "endpoint-value"),
            ("QWEN_TELEMETRY_ENABLED", "true"),
            ("BAD NAME", "x"),
        ]);
        let names: Vec<String> = [
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "OPENAI_MODEL",
            "QWEN_TELEMETRY_ENABLED",
            "BAD NAME",
        ]
        .map(String::from)
        .to_vec();
        let env: HashMap<_, _> = spawn_env(&host, &login, &names).into_iter().collect();
        let get = |k: &str| env.get(k).map(String::as_str);
        assert_eq!(get("PATH"), Some("/opt/homebrew/bin:/usr/bin"));
        assert_eq!(get("HOME"), Some("/Users/x"));
        assert_eq!(get("TERM"), Some("xterm"));
        assert_eq!(get("OPENAI_API_KEY"), Some("from-shell"));
        assert_eq!(get("OPENAI_BASE_URL"), Some("endpoint-value"));
        assert_eq!(get("OPENAI_MODEL"), None, "unset names are skipped");
        assert_eq!(get("AWS_SECRET_ACCESS_KEY"), None);
        assert_eq!(get("ANTHROPIC_API_KEY"), None);
        assert_eq!(get("BAD NAME"), None);
        for (k, v) in [
            ("QWEN_USAGE_STATISTICS_ENABLED", "false"),
            ("QWEN_TELEMETRY_ENABLED", "false"),
            ("QWEN_CODE_MODELS_DEV_REFRESH", "off"),
            ("QWEN_CODE_SKIP_UPDATE_CHECK_ONCE", "true"),
        ] {
            assert_eq!(get(k), Some(v), "{k}");
        }
    }
}
