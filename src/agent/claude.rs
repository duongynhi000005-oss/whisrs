//! The Claude Code driver: each turn is a background session (`claude --bg`).
//!
//! - First turn: `claude --bg --name whisrs-<agent> --permission-mode <m>
//!   --append-system-prompt <instruction> --settings <hooks> [args] -- <text>`.
//!   `--bg` prints `backgrounded · <bg id>`; the bg id is reported (and
//!   persisted) at once, then the session id is read from `claude agents
//!   --json --all` by that bg id (`--session-id` is ignored with `--bg`,
//!   measured on 2.1.301). The appended prompt is recorded on the first
//!   request and reused on every resume, so it is passed once.
//! - Next turn: refused while `claude agents` shows the session `working` (it
//!   runs on its own, and stopping it would lose that work). Otherwise
//!   `claude stop <bg id>` while its process is alive, then `claude --bg
//!   --resume <session id> -- <text>`, which keeps the session id and the
//!   saved options (`--settings` included). Any other flag would start a copy.
//!   A resume that starts a copy anyway prints another bg id; the copy's ids
//!   replace the old ones.
//! - Turn end: the `Stop` hook (`whisrs agent-hook`, installed through
//!   `--settings` on whisrs-started sessions only) carries
//!   `last_assistant_message`. Only a `Stop` received after this turn's launch
//!   counts: the old process of a resumed session can still send one. A
//!   `Notification` of type `permission_prompt` marks the agent needs-you. A
//!   poll of `claude agents --json --all` every ten seconds covers a missing
//!   hook: `done` reads the last assistant text from the transcript (and a
//!   `done` with none for a few polls is an interrupted turn); a vanished or
//!   exited session is an error.
//! - Under a systemd service, `--bg` launches run in a scope of their own
//!   (`systemd-run --user --scope`): the first one can start Claude Code's
//!   background service, which hosts every background session, the user's
//!   own included, and must not die with the whisrs unit.
//!
//! Only ids this driver learned from sessions whisrs started are ever passed
//! to `claude`; nothing is matched by name.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::{
    Agent, AgentPhase, Driver, HookEvent, Permissions, SessionRef, TurnIo, TurnOutcome,
    TurnRequest, TurnUpdate, TurnUpdates, STILL_WORKING,
};

/// How often a running turn is polled.
pub const POLL_INTERVAL: Duration = Duration::from_secs(10);

/// `claude --bg` can start the background service first.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(90);

/// `claude agents --json --all` and `claude stop`.
const QUICK_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a `done` poll waits for the `Stop` hook before reading the
/// transcript instead. Hook events keep being handled meanwhile.
const DONE_GRACE: Duration = Duration::from_secs(2);

/// `done` polls in a row with no assistant text since the turn started before
/// the turn is given up as interrupted (a restart that landed between the
/// stop and the resume, or a resume that never ran).
pub const DONE_POLLS_WITHOUT_TEXT: u32 = 3;

/// The error for a turn that ended without a reply of its own.
pub const INTERRUPTED: &str = "That turn was interrupted, say it again.";

/// Attempts to find a just-launched session in `claude agents`.
const LIST_ATTEMPTS: u32 = 10;

/// The hook timeout written into the settings, in seconds. `whisrs
/// agent-hook` exits within two.
const HOOK_TIMEOUT_SECS: u32 = 5;

/// The Claude Code driver.
pub struct ClaudeDriver {
    /// `'<whisrs>' agent-hook --socket '<socket>'`, run by the session's hooks.
    hook_command: String,
    poll_interval: Duration,
    done_grace: Duration,
    /// Launch `--bg` in a scope of its own ([`in_service_unit`]).
    use_scope: bool,
}

impl ClaudeDriver {
    /// A driver whose sessions report to the daemon listening on `socket`
    /// through the `whisrs` binary at `whisrs_bin`.
    pub fn new(whisrs_bin: &Path, socket: &Path) -> Self {
        Self {
            hook_command: hook_command(whisrs_bin, socket),
            poll_interval: POLL_INTERVAL,
            done_grace: DONE_GRACE,
            use_scope: in_service_unit(),
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// `--permission-mode <mode>` for a harness-neutral setting. `ask` is passed
/// as `default` explicitly: without the flag a session starts in the user's
/// `permissions.defaultMode`, which may well be `auto`.
pub fn permission_args(permissions: Permissions) -> [&'static str; 2] {
    let mode = match permissions {
        Permissions::Auto => "auto",
        Permissions::Bypass => "bypassPermissions",
        Permissions::Ask => "default",
    };
    ["--permission-mode", mode]
}

/// Quote `s` for a POSIX shell (Claude Code runs hook commands through one).
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-+=:,@%".contains(&b))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The hook command line. The socket is explicit because the session runs
/// under Claude Code's background service, whose environment
/// (`XDG_RUNTIME_DIR` included) is not the daemon's.
pub fn hook_command(whisrs_bin: &Path, socket: &Path) -> String {
    format!(
        "{} agent-hook --socket {}",
        shell_quote(&whisrs_bin.to_string_lossy()),
        shell_quote(&socket.to_string_lossy())
    )
}

/// The `--settings` JSON: `Stop` and the `permission_prompt` notification run
/// the hook command. Only sessions whisrs starts get it; the user's own
/// settings files are untouched.
pub fn hook_settings(hook_command: &str) -> String {
    let hook = serde_json::json!([{
        "type": "command",
        "command": hook_command,
        "timeout": HOOK_TIMEOUT_SECS,
    }]);
    serde_json::json!({
        "hooks": {
            "Stop": [{ "hooks": hook }],
            "Notification": [{ "matcher": "permission_prompt", "hooks": hook }],
        }
    })
    .to_string()
}

/// The display name of an agent's sessions in `claude agents`. Display only:
/// sessions are never looked up by it.
pub fn session_name(agent: &Agent) -> String {
    format!("whisrs-{}", agent.name)
}

/// Arguments of a session's first turn. `--` keeps a prompt that starts with
/// a dash from being read as a flag.
pub fn first_turn_args(agent: &Agent, settings: &str, text: &str) -> Vec<String> {
    let mut args: Vec<String> = vec!["--bg".into(), "--name".into(), session_name(agent)];
    args.extend(permission_args(agent.permissions).map(String::from));
    args.push("--append-system-prompt".into());
    args.push(agent.instruction.clone());
    args.push("--settings".into());
    args.push(settings.to_string());
    args.extend(agent.args.iter().cloned());
    args.push("--".into());
    args.push(text.to_string());
    args
}

/// Arguments of a later turn. No other flag: the session keeps its saved
/// options, and any flag passed here makes Claude Code start a copy instead
/// of continuing in place (measured on 2.1.301), so `permissions`, `args` and
/// `reply_instruction` apply from the next new session on.
pub fn resume_args(session_id: &str, text: &str) -> Vec<String> {
    ["--bg", "--resume", session_id, "--", text]
        .map(String::from)
        .to_vec()
}

/// The bg id in `claude --bg`'s output (`backgrounded · 7c5dcf5d · name`).
pub fn parse_backgrounded(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("backgrounded")?;
        let rest = rest.trim_start().strip_prefix('\u{b7}')?;
        let id = rest.split_whitespace().next()?;
        (!id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
            .then(|| id.to_string())
    })
}

/// One background session as `claude agents --json --all` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedSession {
    pub session_id: Option<String>,
    /// `working`, `blocked`, `done`, `failed` or `stopped`. An exited
    /// session keeps the state it had last.
    pub state: Option<String>,
    /// Its process is running (`pid` present).
    pub alive: bool,
    /// What a waiting session waits for (`permission prompt`, ...).
    pub waiting_for: Option<String>,
}

/// Parse `claude agents --json --all` output.
pub fn parse_agent_list(stdout: &str) -> Result<Vec<Value>, String> {
    let start = stdout
        .find('[')
        .ok_or_else(|| "`claude agents --json` printed no JSON list".to_string())?;
    serde_json::from_str::<Vec<Value>>(stdout[start..].trim_end())
        .map_err(|e| format!("`claude agents --json` printed invalid JSON: {e}"))
}

/// The background session with this bg id, if listed.
pub fn find_session(list: &[Value], bg_id: &str) -> Option<ListedSession> {
    let entry = list.iter().find(|e| {
        e.get("id").and_then(Value::as_str) == Some(bg_id)
            && e.get("kind").and_then(Value::as_str) != Some("interactive")
    })?;
    let text = |k: &str| entry.get(k).and_then(Value::as_str).map(str::to_string);
    Some(ListedSession {
        session_id: text("sessionId"),
        state: text("state"),
        alive: entry.get("pid").is_some_and(|p| !p.is_null()),
        waiting_for: text("waitingFor"),
    })
}

/// What a poll of `claude agents` says about the turn's session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollState {
    /// Not listed: removed (`claude rm`).
    Gone,
    /// Listed, but its process is not running (stopped, killed, crashed).
    /// `state` is what it was last; `done` means the turn finished first.
    Exited { state: Option<String> },
    /// Running a turn.
    Working,
    /// Waiting for the user.
    Blocked { waiting_for: Option<String> },
    /// Running and idle: the turn finished.
    Done,
    /// Running, in a state this driver does not know.
    Other,
}

/// Classify a listed session. A running session in a terminal state
/// (`failed`, `stopped`) is treated as exited.
pub fn poll_state(listed: Option<&ListedSession>) -> PollState {
    let Some(listed) = listed else {
        return PollState::Gone;
    };
    if !listed.alive {
        return PollState::Exited {
            state: listed.state.clone(),
        };
    }
    match listed.state.as_deref() {
        Some("working") => PollState::Working,
        Some("blocked") => PollState::Blocked {
            waiting_for: listed.waiting_for.clone(),
        },
        Some("done") => PollState::Done,
        Some("failed" | "stopped") => PollState::Exited {
            state: listed.state.clone(),
        },
        _ => PollState::Other,
    }
}

/// What a parent Claude Code session sets in the environment of the
/// processes it runs. Removed from the `claude` child, so a daemon started
/// from inside a session does not make the agent look like one of its
/// subprocesses. Everything else passes, Claude Code's own configuration
/// included (`CLAUDE_CONFIG_DIR`, `CLAUDE_CODE_OAUTH_TOKEN`,
/// `CLAUDE_CODE_USE_*`, `CLAUDE_CODE_MAX_OUTPUT_TOKENS`, ...).
pub const PARENT_SESSION_VARS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
    "CLAUDE_PROJECT_DIR",
    "CLAUDE_JOB_DIR",
    "AI_AGENT",
];

/// Whether an environment variable is kept from the `claude` child: a
/// parent session's variable ([`PARENT_SESSION_VARS`]), or one of whisrs's
/// own `WHISRS_*` keys (API key overrides): the first `claude --bg` can start
/// Claude Code's background service, which keeps the environment it was
/// started with for every session it hosts.
pub fn strips_env(name: &str) -> bool {
    PARENT_SESSION_VARS.contains(&name) || name.starts_with("WHISRS_")
}

/// The notification and error text for a failed launch.
pub fn launch_failure(agent: &Agent, stdout: &str, stderr: &str) -> String {
    if stderr.contains("Workspace not trusted") || stdout.contains("Workspace not trusted") {
        return format!(
            "Agent '{}': Claude Code does not trust this folder yet. Open {} in Claude Code once \
             and accept the trust prompt.",
            agent.name,
            agent.dir.display()
        );
    }
    let detail = stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("warning:") && !l.starts_with("note:"))
        .unwrap_or("no output");
    format!(
        "Agent '{}': `{} --bg` failed: {detail}",
        agent.name, agent.command
    )
}

/// The error for a binary that cannot be started. Spawning in a missing
/// working directory fails with "not found" too, so the dir is checked first
/// (`dir_exists`).
pub fn spawn_failure(agent: &Agent, error: &std::io::Error, dir_exists: bool) -> String {
    if !dir_exists {
        format!(
            "Agent '{}': its dir {} does not exist.",
            agent.name,
            agent.dir.display()
        )
    } else if error.kind() == std::io::ErrorKind::NotFound {
        format!(
            "Agent '{}': `{}` was not found. Install Claude Code, or set command in [[agents]] \
             to its full path (a systemd user service may not have ~/.local/bin on PATH).",
            agent.name, agent.command
        )
    } else {
        format!(
            "Agent '{}': cannot run `{}`: {error}",
            agent.name, agent.command
        )
    }
}

/// Whether a `/proc/self/cgroup` listing puts this process in a systemd
/// service (`.../whisrs.service`), as opposed to a terminal's scope.
pub fn cgroup_is_service(cgroup: &str) -> bool {
    cgroup.lines().any(|line| {
        line.trim_end()
            .rsplit('/')
            .next()
            .is_some_and(|leaf| leaf.ends_with(".service"))
    })
}

/// Whether this daemon runs as a systemd service. Then a `claude --bg` it
/// starts directly could start Claude Code's background service inside the
/// whisrs unit, and a restart of whisrs (`KillMode=control-group`) would
/// kill it with every session it hosts. Checked once.
fn in_service_unit() -> bool {
    static IN_SERVICE: OnceLock<bool> = OnceLock::new();
    *IN_SERVICE.get_or_init(|| {
        std::env::var_os("INVOCATION_ID").is_some_and(|v| !v.is_empty())
            || std::fs::read_to_string("/proc/self/cgroup").is_ok_and(|c| cgroup_is_service(&c))
    })
}

/// `systemd-run` arguments that run `command args` in a transient scope of
/// its own. The scope keeps the environment, the working directory and the
/// output pipes (`systemd-run --scope` execs the command itself).
pub fn scope_args(command: &str, args: &[String]) -> Vec<String> {
    let mut out: Vec<String> = ["--user", "--scope", "--quiet", "--collect", "--"]
        .map(String::from)
        .to_vec();
    out.push(command.to_string());
    out.extend(args.iter().cloned());
    out
}

/// Whether a failed `systemd-run --scope` failed itself (no user manager,
/// no executable), so the command never ran and can be started directly.
pub fn scope_unavailable(success: bool, stdout: &str, stderr: &str) -> bool {
    if success || !stdout.trim().is_empty() {
        return false;
    }
    stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .is_some_and(|first| {
            first.starts_with("Failed to ")
                && ["bus", "transient", "scope", "executable"]
                    .iter()
                    .any(|w| first.contains(w))
        })
}

/// What a hook payload means to a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookKind {
    /// The turn ended.
    Stop {
        message: Option<String>,
        transcript_path: Option<PathBuf>,
    },
    /// A permission prompt waits.
    PermissionPrompt,
    /// Anything else (idle notifications and the like).
    Other,
}

pub fn classify_hook(payload: &Value) -> HookKind {
    let event = payload.get("hook_event_name").and_then(Value::as_str);
    match event {
        Some("Stop") => HookKind::Stop {
            message: payload
                .get("last_assistant_message")
                .and_then(Value::as_str)
                .map(str::to_string),
            transcript_path: payload
                .get("transcript_path")
                .and_then(Value::as_str)
                .map(PathBuf::from),
        },
        Some("Notification")
            if payload.get("notification_type").and_then(Value::as_str)
                == Some("permission_prompt") =>
        {
            HookKind::PermissionPrompt
        }
        _ => HookKind::Other,
    }
}

/// Whether a hook event belongs to the process this turn launched: it was
/// received at or after `accept_after` (the launch). `None` accepts every
/// event (a turn watched again after a restart).
pub fn is_current(event: &HookEvent, accept_after: Option<Instant>) -> bool {
    accept_after.is_none_or(|after| event.received >= after)
}

/// The text of the last assistant message in a transcript (JSONL) written at
/// or after `since`, skipping subagent (sidechain) messages. `None` when the
/// turn has no assistant text yet.
pub fn last_assistant_text(transcript: &str, since: DateTime<Utc>) -> Option<String> {
    let mut last = None;
    for line in transcript.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) != Some("assistant")
            || entry.get("isSidechain").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let at = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc));
        if at.is_none_or(|at| at < since) {
            continue;
        }
        let Some(content) = entry.pointer("/message/content").and_then(Value::as_array) else {
            continue;
        };
        let text: Vec<&str> = content
            .iter()
            .filter(|c| c.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|c| c.get("text").and_then(Value::as_str))
            .collect();
        if !text.is_empty() {
            last = Some(text.join("\n"));
        }
    }
    last
}

/// The name of the last tool the assistant asked to use in a transcript.
pub fn last_tool_use(transcript: &str) -> Option<String> {
    let mut last = None;
    for line in transcript.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if let Some(content) = entry.pointer("/message/content").and_then(Value::as_array) {
            for c in content {
                if c.get("type").and_then(Value::as_str) == Some("tool_use") {
                    if let Some(name) = c.get("name").and_then(Value::as_str) {
                        last = Some(name.to_string());
                    }
                }
            }
        }
    }
    last
}

/// The spoken notice for a permission prompt.
pub fn needs_you_notice(tool: Option<&str>) -> String {
    match tool {
        Some(tool) if spoken_tool_name(tool).is_some() => format!(
            "Waiting for your approval to use {}.",
            spoken_tool_name(tool).unwrap_or_default()
        ),
        _ => "Waiting for your approval.".to_string(),
    }
}

/// A tool name worth saying: plain words, not an MCP server's
/// `mcp__server__tool` id.
fn spoken_tool_name(tool: &str) -> Option<String> {
    if tool.starts_with("mcp__") || tool.is_empty() || tool.len() > 32 {
        return None;
    }
    tool.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == ' ')
        .then(|| tool.to_string())
}

/// `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
fn claude_config_dir() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".claude")))
}

/// Find a session's transcript by its id: `<config>/projects/*/<id>.jsonl`.
/// Only our own session's file is ever opened. Blocking.
fn find_transcript(session_id: &str) -> Option<PathBuf> {
    let projects = claude_config_dir()?.join("projects");
    let file = format!("{session_id}.jsonl");
    std::fs::read_dir(projects)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path().join(&file))
        .find(|p| p.is_file())
}

/// Read and parse the session's transcript off the async runtime: a long
/// session's JSONL is megabytes.
async fn with_transcript<T: Send + 'static>(
    session: &SessionRef,
    parse: impl FnOnce(&str) -> Option<T> + Send + 'static,
) -> Option<T> {
    let path = session.transcript_path.clone();
    let session_id = session.session_id.clone();
    tokio::task::spawn_blocking(move || {
        let path = path.or_else(|| session_id.as_deref().and_then(find_transcript))?;
        let contents = std::fs::read_to_string(path).ok()?;
        parse(&contents)
    })
    .await
    .ok()
    .flatten()
}

/// The last assistant text of this turn, from the transcript.
async fn transcript_reply(session: &SessionRef, since: DateTime<Utc>) -> Option<String> {
    with_transcript(session, move |t| last_assistant_text(t, since)).await
}

/// The needs-you notice, naming the tool when the transcript shows it.
async fn notice(session: &SessionRef) -> String {
    let tool = with_transcript(session, last_tool_use).await;
    needs_you_notice(tool.as_deref())
}

// ---------------------------------------------------------------------------
// Process plumbing
// ---------------------------------------------------------------------------

/// Captured output of a finished process.
struct Output {
    success: bool,
    stdout: String,
    stderr: String,
}

/// Why a process gave no output.
enum ExecError {
    Spawn(std::io::Error),
    Wait(std::io::Error),
    Timeout,
}

/// Run `program args` in the agent's dir, stdin closed, the parent session's
/// variables removed, killed if dropped or past `timeout`.
async fn exec(
    agent: &Agent,
    program: &str,
    args: &[String],
    timeout: Duration,
) -> Result<Output, ExecError> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(&agent.dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (name, _) in std::env::vars_os() {
        if let Some(name) = name.to_str() {
            if strips_env(name) {
                cmd.env_remove(name);
            }
        }
    }
    let child = cmd.spawn().map_err(ExecError::Spawn)?;
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => Ok(Output {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }),
        Ok(Err(e)) => Err(ExecError::Wait(e)),
        Err(_) => Err(ExecError::Timeout),
    }
}

/// The message for an [`ExecError`] of the agent's `claude` run with `args`.
fn exec_failure(agent: &Agent, args: &[String], timeout: Duration, error: ExecError) -> String {
    match error {
        ExecError::Spawn(e) => spawn_failure(agent, &e, agent.dir.is_dir()),
        ExecError::Wait(e) => format!("Agent '{}': `{}` failed: {e}", agent.name, agent.command),
        ExecError::Timeout => format!(
            "Agent '{}': `{} {}` did not finish in {} seconds",
            agent.name,
            agent.command,
            args.first().map(String::as_str).unwrap_or(""),
            timeout.as_secs()
        ),
    }
}

/// Run the agent's `claude` with `args`.
async fn run_claude(agent: &Agent, args: &[String], timeout: Duration) -> Result<Output, String> {
    exec(agent, &agent.command, args, timeout)
        .await
        .map_err(|e| exec_failure(agent, args, timeout, e))
}

/// Run a `claude --bg` launch: in a scope of its own when `use_scope` (this
/// daemon is a systemd service, [`in_service_unit`]), directly otherwise or
/// when `systemd-run` is missing or cannot create the scope.
async fn run_launch(agent: &Agent, args: &[String], use_scope: bool) -> Result<Output, String> {
    if use_scope {
        let scoped = scope_args(&agent.command, args);
        match exec(agent, "systemd-run", &scoped, LAUNCH_TIMEOUT).await {
            Ok(out) if !scope_unavailable(out.success, &out.stdout, &out.stderr) => {
                debug!("agent '{}': launched in a scope of its own", agent.name);
                return Ok(out);
            }
            Ok(out) => warn!(
                "agent '{}': cannot start claude in a scope of its own ({}); starting it \
                 directly, so Claude Code's background service may run inside the whisrs unit",
                agent.name,
                out.stderr.lines().next().unwrap_or("").trim()
            ),
            Err(ExecError::Spawn(e)) => {
                debug!(
                    "agent '{}': systemd-run unavailable ({e}); starting claude directly",
                    agent.name
                );
            }
            Err(e) => return Err(exec_failure(agent, args, LAUNCH_TIMEOUT, e)),
        }
    }
    run_claude(agent, args, LAUNCH_TIMEOUT).await
}

/// `claude agents --json --all`, parsed.
async fn list_sessions(agent: &Agent) -> Result<Vec<Value>, String> {
    let args = ["agents", "--json", "--all"].map(String::from);
    let out = run_claude(agent, &args, QUICK_TIMEOUT).await?;
    if !out.success {
        return Err(format!(
            "`claude agents --json --all` failed: {}",
            out.stderr.trim()
        ));
    }
    parse_agent_list(&out.stdout)
}

/// Look a just-launched bg id up until `claude agents` lists it.
async fn learn_session(agent: &Agent, bg_id: &str) -> Result<ListedSession, String> {
    let mut last_error = None;
    for attempt in 0..LIST_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        match list_sessions(agent).await {
            Ok(list) => {
                if let Some(found) = find_session(&list, bg_id) {
                    if found.session_id.is_some() {
                        return Ok(found);
                    }
                }
            }
            Err(e) => last_error = Some(e),
        }
    }
    Err(format!(
        "Agent '{}': Claude Code started session {bg_id} but does not list it{}",
        agent.name,
        last_error.map(|e| format!(" ({e})")).unwrap_or_default()
    ))
}

/// Launch `args` and return the bg id it printed.
async fn launch(agent: &Agent, args: &[String], use_scope: bool) -> Result<String, String> {
    let out = run_launch(agent, args, use_scope).await?;
    for line in out.stderr.lines().filter(|l| !l.trim().is_empty()) {
        info!("agent '{}': claude: {}", agent.name, line.trim());
    }
    if !out.success {
        return Err(launch_failure(agent, &out.stdout, &out.stderr));
    }
    parse_backgrounded(&out.stdout).ok_or_else(|| {
        format!(
            "Agent '{}': `{} --bg` printed no session id: {}",
            agent.name,
            agent.command,
            out.stdout.trim()
        )
    })
}

/// `claude stop <bg id>`.
async fn stop_bg(agent: &Agent, bg_id: &str) -> Result<(), String> {
    let args = ["stop".to_string(), bg_id.to_string()];
    let out = run_claude(agent, &args, QUICK_TIMEOUT).await?;
    if out.success {
        debug!("agent '{}': stopped bg {bg_id}", agent.name);
        Ok(())
    } else {
        Err(format!(
            "`claude stop {bg_id}` failed: {}",
            out.stderr.trim()
        ))
    }
}

/// A launch that runs on a task of its own, so stopping the turn cannot cut
/// it off between starting a session and reporting it. The bg id is
/// reported (`base` with that bg id) as soon as it is printed, before the
/// session id is looked up, so `--stop` and a restart know it. When the turn
/// was stopped meanwhile, the launch stops the session it started.
async fn launch_reported(
    agent: &Agent,
    args: Vec<String>,
    base: SessionRef,
    updates: &TurnUpdates,
    use_scope: bool,
) -> Result<(String, SessionRef), String> {
    let agent = agent.clone();
    let updates = updates.clone();
    let task = tokio::spawn(async move {
        let bg_id = launch(&agent, &args, use_scope).await?;
        let mut session = base;
        session.bg_id = Some(bg_id.clone());
        if updates.report_session(session.clone()) {
            info!(
                "agent '{}': the turn was stopped during the launch; stopping bg {bg_id}",
                agent.name
            );
            if let Err(e) = stop_bg(&agent, &bg_id).await {
                warn!("agent '{}': {e}", agent.name);
            }
            return Err(format!("Agent '{}': the turn was stopped.", agent.name));
        }
        Ok((bg_id, session))
    });
    task.await
        .map_err(|e| format!("the launch task failed: {e}"))?
}

impl ClaudeDriver {
    /// The first turn of a new session, and when its launch was spawned.
    async fn first_turn(
        &self,
        agent: &Agent,
        text: &str,
        updates: &TurnUpdates,
    ) -> Result<(SessionRef, Instant), String> {
        let settings = hook_settings(&self.hook_command);
        let launched_at = Instant::now();
        let (bg_id, mut session) = launch_reported(
            agent,
            first_turn_args(agent, &settings, text),
            SessionRef::default(),
            updates,
            self.use_scope,
        )
        .await?;
        match learn_session(agent, &bg_id).await {
            Ok(listed) => {
                session.session_id = listed.session_id;
                info!(
                    "agent '{}': started session {} (bg {bg_id})",
                    agent.name,
                    session.session_id.as_deref().unwrap_or("?")
                );
            }
            // Watched by its bg id: the poll learns the session id.
            Err(e) => warn!("{e}; watching it by its bg id"),
        }
        Ok((session, launched_at))
    }

    /// A later turn: refused while the session works on its own; otherwise
    /// stop the old process if it is alive, then resume. Returns the session
    /// and when the resume was spawned: hook events received earlier are the
    /// old process's.
    async fn next_turn(
        &self,
        agent: &Agent,
        session: &SessionRef,
        text: &str,
        updates: &TurnUpdates,
    ) -> Result<(SessionRef, Instant), String> {
        let mut session_id = session.session_id.clone();
        if let Some(bg_id) = &session.bg_id {
            match list_sessions(agent).await {
                Ok(list) => match find_session(&list, bg_id) {
                    Some(listed) => {
                        if session_id.is_none() {
                            session_id = listed.session_id.clone();
                        }
                        if poll_state(Some(&listed)) == PollState::Working {
                            return Err(format!("Agent '{}': {STILL_WORKING}", agent.name));
                        }
                        if listed.alive {
                            if let Err(e) = stop_bg(agent, bg_id).await {
                                warn!("agent '{}': {e}", agent.name);
                            }
                        }
                    }
                    None => {
                        // Removed (`claude rm`): a resume would come back
                        // without its saved options, hooks included.
                        info!(
                            "agent '{}': session {} is no longer listed; starting a new one",
                            agent.name,
                            session_id.as_deref().unwrap_or(bg_id)
                        );
                        return self.first_turn(agent, text, updates).await;
                    }
                },
                // Cannot tell: stopping a stopped session is harmless.
                Err(e) => {
                    debug!("agent '{}': {e}", agent.name);
                    if let Err(e) = stop_bg(agent, bg_id).await {
                        debug!("agent '{}': {e}", agent.name);
                    }
                }
            }
        }
        let Some(session_id) = session_id else {
            return self.first_turn(agent, text, updates).await;
        };
        let base = SessionRef {
            session_id: Some(session_id.clone()),
            ..session.clone()
        };
        let launched_at = Instant::now();
        let (bg_id, resumed) = launch_reported(
            agent,
            resume_args(&session_id, text),
            base,
            updates,
            self.use_scope,
        )
        .await?;
        if session.bg_id.as_deref() == Some(bg_id.as_str()) {
            return Ok((resumed, launched_at));
        }
        // A copy: Claude Code could not continue in place.
        let listed = learn_session(agent, &bg_id).await?;
        warn!(
            "agent '{}': resume started a copy: session {} (bg {bg_id}) replaces {session_id}",
            agent.name,
            listed.session_id.as_deref().unwrap_or("?")
        );
        Ok((
            SessionRef {
                session_id: listed.session_id,
                bg_id: Some(bg_id),
                transcript_path: None,
            },
            launched_at,
        ))
    }

    /// Wait for the turn of `session` to end. Hook events received before
    /// `accept_after` belong to an earlier process and are ignored.
    async fn watch(
        &self,
        agent: &Agent,
        mut session: SessionRef,
        since: DateTime<Utc>,
        accept_after: Option<Instant>,
        hooks: &mut mpsc::UnboundedReceiver<HookEvent>,
        updates: &TurnUpdates,
    ) -> TurnOutcome {
        let Some(bg_id) = session.bg_id.clone() else {
            return TurnOutcome::Failed(format!(
                "Agent '{}': {INTERRUPTED} It stopped before its session started.",
                agent.name
            ));
        };
        let mut poll = tokio::time::interval(self.poll_interval);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick is immediate; the hook usually comes first.
        poll.tick().await;
        let mut needs_you = false;
        let mut hooks_open = true;
        // Set by a `done` poll: when to read the transcript if the Stop hook
        // has not come by then.
        let mut done_deadline: Option<tokio::time::Instant> = None;
        let mut done_without_text = 0u32;
        loop {
            let deadline = done_deadline
                .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(86_400));
            tokio::select! {
                event = hooks.recv(), if hooks_open => {
                    let Some(event) = event else {
                        hooks_open = false;
                        continue;
                    };
                    if !is_current(&event, accept_after) {
                        debug!(
                            "agent '{}': ignored a hook event from before this turn's launch",
                            agent.name
                        );
                        continue;
                    }
                    match classify_hook(&event.payload) {
                        HookKind::Stop { message, transcript_path } => {
                            if let Some(path) = transcript_path {
                                if session.transcript_path.as_ref() != Some(&path) {
                                    session.transcript_path = Some(path);
                                    updates.send(TurnUpdate::Session(session.clone()));
                                }
                            }
                            let text = match message {
                                Some(m) if !m.trim().is_empty() => m,
                                _ => transcript_reply(&session, since).await.unwrap_or_default(),
                            };
                            return TurnOutcome::Reply(text);
                        }
                        HookKind::PermissionPrompt => {
                            if !needs_you {
                                needs_you = true;
                                updates.send(TurnUpdate::NeedsYou(notice(&session).await));
                            }
                        }
                        HookKind::Other => {}
                    }
                }
                _ = tokio::time::sleep_until(deadline), if done_deadline.is_some() => {
                    done_deadline = None;
                    if let Some(text) = transcript_reply(&session, since).await {
                        info!(
                            "agent '{}': turn done without a Stop hook; read the transcript",
                            agent.name
                        );
                        return TurnOutcome::Reply(text);
                    }
                    // `done` before this turn wrote anything: the previous
                    // turn's state for a moment, or a turn that never ran.
                    done_without_text += 1;
                    if done_without_text >= DONE_POLLS_WITHOUT_TEXT {
                        warn!(
                            "agent '{}': the session is done but this turn wrote nothing",
                            agent.name
                        );
                        return TurnOutcome::Failed(format!("Agent '{}': {INTERRUPTED}", agent.name));
                    }
                }
                _ = poll.tick() => {
                    let list = match list_sessions(agent).await {
                        Ok(list) => list,
                        Err(e) => {
                            warn!("agent '{}': poll failed: {e}", agent.name);
                            continue;
                        }
                    };
                    let listed = find_session(&list, &bg_id);
                    // A launch watched again after a restart may not know
                    // its session id yet: learn it, so hooks match.
                    if let Some(id) = listed.as_ref().and_then(|l| l.session_id.clone()) {
                        if session.session_id.is_none() {
                            session.session_id = Some(id);
                            updates.send(TurnUpdate::Session(session.clone()));
                        }
                    }
                    match poll_state(listed.as_ref()) {
                        PollState::Gone => {
                            // Forget it: the next turn starts a new session.
                            updates.send(TurnUpdate::Session(SessionRef::default()));
                            return TurnOutcome::Failed(format!(
                                "Agent '{}': the session ended, Claude Code no longer lists it. \
                                 The next turn starts a new one.",
                                agent.name
                            ));
                        }
                        PollState::Exited { state } => {
                            if state.as_deref() == Some("done") {
                                if let Some(text) = transcript_reply(&session, since).await {
                                    return TurnOutcome::Reply(text);
                                }
                            }
                            if state.as_deref() == Some("failed") {
                                return TurnOutcome::Failed(format!(
                                    "Agent '{}': the session failed. Open it with `claude attach \
                                     {bg_id}` to see why.",
                                    agent.name
                                ));
                            }
                            return TurnOutcome::Failed(format!(
                                "Agent '{}': the session ended before replying. The next turn \
                                 continues it.",
                                agent.name
                            ));
                        }
                        PollState::Blocked { waiting_for } => {
                            done_without_text = 0;
                            done_deadline = None;
                            if !needs_you {
                                needs_you = true;
                                let notice = match waiting_for.as_deref() {
                                    Some("permission prompt") | None => notice(&session).await,
                                    Some(_) => "The agent needs you.".to_string(),
                                };
                                updates.send(TurnUpdate::NeedsYou(notice));
                            }
                        }
                        PollState::Working => {
                            done_without_text = 0;
                            done_deadline = None;
                            if needs_you {
                                needs_you = false;
                                updates.send(TurnUpdate::Working);
                            }
                        }
                        PollState::Done => {
                            // Give the Stop hook a moment: it carries the
                            // message without reading the transcript.
                            if done_deadline.is_none() {
                                done_deadline =
                                    Some(tokio::time::Instant::now() + self.done_grace);
                            }
                        }
                        PollState::Other => {}
                    }
                }
            }
        }
    }
}

#[async_trait]
impl Driver for ClaudeDriver {
    async fn run_turn(
        &self,
        agent: &Agent,
        session: SessionRef,
        request: TurnRequest,
        io: TurnIo,
    ) -> TurnOutcome {
        let TurnIo { mut hooks, updates } = io;
        if !agent.dir.is_dir() {
            return TurnOutcome::Failed(format!(
                "Agent '{}': its dir {} no longer exists.",
                agent.name,
                agent.dir.display()
            ));
        }
        let launched = if session.session_id.is_some() || session.bg_id.is_some() {
            self.next_turn(agent, &session, &request.text, &updates)
                .await
        } else {
            self.first_turn(agent, &request.text, &updates).await
        };
        let (session, launched_at) = match launched {
            Ok(s) => s,
            Err(message) => return TurnOutcome::Failed(message),
        };
        updates.send(TurnUpdate::Session(session.clone()));
        self.watch(
            agent,
            session,
            request.started_at,
            Some(launched_at),
            &mut hooks,
            &updates,
        )
        .await
    }

    async fn rewatch(
        &self,
        agent: &Agent,
        session: SessionRef,
        request: TurnRequest,
        io: TurnIo,
    ) -> TurnOutcome {
        let TurnIo { mut hooks, updates } = io;
        // The old daemon saw the launch; every event that reaches this one
        // is the running process's.
        self.watch(
            agent,
            session,
            request.started_at,
            None,
            &mut hooks,
            &updates,
        )
        .await
    }

    async fn stop(&self, agent: &Agent, session: &SessionRef) -> Result<(), String> {
        match &session.bg_id {
            Some(bg_id) => stop_bg(agent, bg_id).await,
            None => Ok(()),
        }
    }

    fn hook_matches(&self, payload: &Value, session: &SessionRef) -> bool {
        match (
            payload.get("session_id").and_then(Value::as_str),
            session.session_id.as_deref(),
        ) {
            (Some(theirs), Some(ours)) => theirs == ours,
            _ => false,
        }
    }

    async fn session_phase(
        &self,
        agent: &Agent,
        session: &SessionRef,
    ) -> Result<AgentPhase, String> {
        let Some(bg_id) = &session.bg_id else {
            return Ok(AgentPhase::Idle);
        };
        let list = list_sessions(agent).await?;
        Ok(match poll_state(find_session(&list, bg_id).as_ref()) {
            PollState::Working => AgentPhase::Working,
            PollState::Blocked { .. } => AgentPhase::NeedsYou,
            _ => AgentPhase::Idle,
        })
    }

    fn idle_reply(&self, payload: &Value) -> Option<String> {
        match classify_hook(payload) {
            HookKind::Stop {
                message: Some(message),
                ..
            } if !message.trim().is_empty() => Some(message),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Harness, DEFAULT_REPLY_INSTRUCTION};
    use std::os::unix::fs::PermissionsExt;

    fn agent(permissions: Permissions) -> Agent {
        Agent {
            name: "jarvis".into(),
            hotkey: None,
            harness: Harness::Claude,
            dir: PathBuf::from("/proj"),
            permissions,
            command: "claude".into(),
            args: vec!["--model".into(), "haiku".into()],
            instruction: DEFAULT_REPLY_INSTRUCTION.into(),
        }
    }

    #[test]
    fn permissions_map_to_claude_modes() {
        assert_eq!(
            permission_args(Permissions::Auto),
            ["--permission-mode", "auto"]
        );
        assert_eq!(
            permission_args(Permissions::Bypass),
            ["--permission-mode", "bypassPermissions"]
        );
        assert_eq!(
            permission_args(Permissions::Ask),
            ["--permission-mode", "default"]
        );
    }

    #[test]
    fn first_turn_passes_everything_once_and_ends_with_the_prompt() {
        let a = agent(Permissions::Auto);
        let args = first_turn_args(&a, "{}", "-rf is not a flag here");
        assert_eq!(
            args,
            [
                "--bg",
                "--name",
                "whisrs-jarvis",
                "--permission-mode",
                "auto",
                "--append-system-prompt",
                DEFAULT_REPLY_INSTRUCTION,
                "--settings",
                "{}",
                "--model",
                "haiku",
                "--",
                "-rf is not a flag here",
            ]
        );
    }

    #[test]
    fn a_resume_passes_no_flag_but_the_session() {
        assert_eq!(
            resume_args("cc7a37cf-9343", "-next"),
            ["--bg", "--resume", "cc7a37cf-9343", "--", "-next"]
        );
    }

    #[test]
    fn the_hook_settings_run_the_hook_on_stop_and_permission_prompts() {
        let cmd = hook_command(
            Path::new("/usr/bin/whisrs"),
            Path::new("/run/user/1000/whisrs.sock"),
        );
        assert_eq!(
            cmd,
            "/usr/bin/whisrs agent-hook --socket /run/user/1000/whisrs.sock"
        );
        let settings: Value = serde_json::from_str(&hook_settings(&cmd)).unwrap();
        assert_eq!(
            settings.pointer("/hooks/Stop/0/hooks/0/command"),
            Some(&Value::from(cmd.clone()))
        );
        assert_eq!(
            settings.pointer("/hooks/Notification/0/matcher"),
            Some(&Value::from("permission_prompt"))
        );
        assert_eq!(
            settings.pointer("/hooks/Notification/0/hooks/0/command"),
            Some(&Value::from(cmd))
        );
        assert_eq!(
            settings.pointer("/hooks/Stop/0/hooks/0/timeout"),
            Some(&Value::from(HOOK_TIMEOUT_SECS))
        );
    }

    #[test]
    fn hook_paths_are_shell_quoted() {
        assert_eq!(shell_quote("/a/b-c_d.e"), "/a/b-c_d.e");
        assert_eq!(shell_quote("/my dir/whisrs"), "'/my dir/whisrs'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn the_bg_id_is_read_from_the_launch_output() {
        let out = "Starting background service\u{2026}\nbackgrounded \u{b7} cc7a37cf \u{b7} whisrs-probe1\n  claude agents             list sessions\n";
        assert_eq!(parse_backgrounded(out).as_deref(), Some("cc7a37cf"));
        assert_eq!(
            parse_backgrounded("backgrounded \u{b7} 7c5dcf5d\n").as_deref(),
            Some("7c5dcf5d")
        );
        assert_eq!(parse_backgrounded("nothing here"), None);
    }

    /// Real `claude agents --json --all` output (2.1.301), trimmed.
    const LIST: &str = r#"[
      {"id":"35622b7b","cwd":"/home/u/Career","kind":"background","startedAt":1,"sessionId":"35622b7b-c088","name":"whisrs-jarvis","state":"blocked"},
      {"pid":1690246,"id":"cc7a37cf","cwd":"/proj","kind":"background","startedAt":2,"sessionId":"cc7a37cf-9343","name":"whisrs-probe1","status":"idle","state":"done"},
      {"pid":1690243,"id":"e237a624","cwd":"/proj","kind":"background","startedAt":3,"sessionId":"e237a624-7f3b","name":"whisrs-probe2","status":"waiting","waitingFor":"permission prompt","state":"blocked"},
      {"pid":1522977,"cwd":"/home/u/whisrs","kind":"interactive","startedAt":4,"sessionId":"ddc53afe-477f","name":"whisrs-d9","status":"busy"}
    ]"#;

    #[test]
    fn sessions_are_found_by_bg_id_only() {
        let list = parse_agent_list(LIST).unwrap();
        assert_eq!(
            find_session(&list, "cc7a37cf"),
            Some(ListedSession {
                session_id: Some("cc7a37cf-9343".into()),
                state: Some("done".into()),
                alive: true,
                waiting_for: None,
            })
        );
        let blocked = find_session(&list, "e237a624").unwrap();
        assert_eq!(blocked.waiting_for.as_deref(), Some("permission prompt"));
        let exited = find_session(&list, "35622b7b").unwrap();
        assert!(!exited.alive);
        // Names are never matched, and interactive sessions have no bg id.
        assert_eq!(find_session(&list, "whisrs-jarvis"), None);
        assert_eq!(find_session(&list, "ddc53afe"), None);
    }

    /// An exited entry keeps its last state (`35622b7b` has no pid but says
    /// `blocked`): the poll goes by the process, not the state.
    #[test]
    fn the_poll_goes_by_the_process_first() {
        let list = parse_agent_list(LIST).unwrap();
        assert_eq!(
            poll_state(find_session(&list, "35622b7b").as_ref()),
            PollState::Exited {
                state: Some("blocked".into())
            }
        );
        assert_eq!(
            poll_state(find_session(&list, "cc7a37cf").as_ref()),
            PollState::Done
        );
        assert_eq!(
            poll_state(find_session(&list, "e237a624").as_ref()),
            PollState::Blocked {
                waiting_for: Some("permission prompt".into())
            }
        );
        assert_eq!(
            poll_state(find_session(&list, "nope").as_ref()),
            PollState::Gone
        );
        let listed = |state: &str, alive: bool| ListedSession {
            session_id: None,
            state: Some(state.into()),
            alive,
            waiting_for: None,
        };
        assert_eq!(
            poll_state(Some(&listed("working", true))),
            PollState::Working
        );
        assert_eq!(
            poll_state(Some(&listed("working", false))),
            PollState::Exited {
                state: Some("working".into())
            }
        );
        assert_eq!(
            poll_state(Some(&listed("stopped", true))),
            PollState::Exited {
                state: Some("stopped".into())
            }
        );
    }

    #[test]
    fn the_list_parser_skips_a_preface() {
        let out = format!("Starting background service\u{2026}\n{LIST}\n");
        assert_eq!(parse_agent_list(&out).unwrap().len(), 4);
        assert!(parse_agent_list("no json").is_err());
    }

    /// Exactly what a parent session sets (measured from a hook's
    /// environment on 2.1.301) and whisrs's own keys go; Claude Code's own
    /// configuration passes.
    #[test]
    fn only_a_parent_sessions_variables_are_stripped() {
        for name in [
            "CLAUDECODE",
            "CLAUDE_CODE_ENTRYPOINT",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDE_CODE_SESSION_ATTENDED",
            "CLAUDE_CODE_MESSAGING_SOCKET",
            "CLAUDE_CODE_MESSAGING_TOKEN",
            "CLAUDE_CODE_SSE_PORT",
            "CLAUDE_CODE_EXECPATH",
            "CLAUDE_PID",
            "CLAUDE_EFFORT",
            "CLAUDE_PROJECT_DIR",
            "CLAUDE_JOB_DIR",
            "AI_AGENT",
            "WHISRS_OPENAI_API_KEY",
            "WHISRS_DEEPGRAM_API_KEY",
        ] {
            assert!(strips_env(name), "{name}");
        }
        for name in [
            "CLAUDE_CONFIG_DIR",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_SKIP_VERTEX_AUTH",
            "CLAUDE_CODE_SOMETHING_NEW",
            "CLAUDE_SOMETHING",
            "PATH",
            "HOME",
            "ANTHROPIC_API_KEY",
        ] {
            assert!(!strips_env(name), "{name}");
        }
    }

    #[test]
    fn an_untrusted_dir_gets_the_trust_hint() {
        let a = agent(Permissions::Auto);
        let stderr = "Workspace not trusted. Run `claude` in /proj once and accept the trust \
                      prompt, then retry.\n";
        assert_eq!(
            launch_failure(&a, "", stderr),
            "Agent 'jarvis': Claude Code does not trust this folder yet. Open /proj in Claude \
             Code once and accept the trust prompt."
        );
        assert_eq!(
            launch_failure(&a, "", "warning: x\nerror: boom\n"),
            "Agent 'jarvis': `claude --bg` failed: error: boom"
        );
    }

    #[test]
    fn a_missing_binary_names_the_command() {
        let a = agent(Permissions::Auto);
        let e = std::io::Error::from(std::io::ErrorKind::NotFound);
        let msg = spawn_failure(&a, &e, true);
        assert!(msg.contains("`claude` was not found"), "{msg}");
        assert!(msg.contains("set command in [[agents]]"), "{msg}");
    }

    /// Spawning in a missing dir also fails with "not found": the dir is
    /// named, not the binary.
    #[test]
    fn a_missing_dir_is_reported_as_the_dir() {
        let a = agent(Permissions::Auto);
        let e = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(
            spawn_failure(&a, &e, false),
            "Agent 'jarvis': its dir /proj does not exist."
        );
    }

    #[test]
    fn a_service_cgroup_is_told_from_a_terminal_scope() {
        assert!(cgroup_is_service(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/whisrs.service\n"
        ));
        assert!(!cgroup_is_service(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/tmux-spawn-ab.scope\n"
        ));
        assert!(!cgroup_is_service(
            "0::/user.slice/user-1000.slice/user@1000.service/init.scope\n"
        ));
        assert!(!cgroup_is_service(""));
    }

    #[test]
    fn a_scoped_launch_wraps_the_command_unchanged() {
        let args = vec!["--bg".to_string(), "--".to_string(), "-x".to_string()];
        assert_eq!(
            scope_args("/usr/bin/claude", &args),
            [
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--",
                "/usr/bin/claude",
                "--bg",
                "--",
                "-x"
            ]
        );
    }

    /// systemd-run's own failures fall back to a direct launch; a failure of
    /// claude itself (which ran) does not launch it twice.
    #[test]
    fn only_systemd_runs_own_failures_fall_back() {
        assert!(scope_unavailable(
            false,
            "",
            "Failed to connect to user scope bus via local transport: No such file or directory\n"
        ));
        assert!(scope_unavailable(
            false,
            "",
            "Failed to start transient scope unit: Access denied\n"
        ));
        assert!(scope_unavailable(
            false,
            "",
            "Failed to find executable claude: No such file or directory\n"
        ));
        assert!(!scope_unavailable(false, "", "Workspace not trusted.\n"));
        assert!(!scope_unavailable(false, "", "error: boom\n"));
        assert!(!scope_unavailable(
            false,
            "backgrounded \u{b7} x\n",
            "Failed to connect to bus\n"
        ));
        assert!(!scope_unavailable(true, "", "Failed to connect to bus\n"));
    }

    /// Real hook payloads (2.1.301, trimmed).
    #[test]
    fn hook_payloads_are_classified() {
        let stop: Value = serde_json::from_str(
            r#"{"session_id":"cc7a37cf-9343","transcript_path":"/t/cc7a37cf-9343.jsonl","cwd":"/proj","permission_mode":"auto","hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":"pong"}"#,
        )
        .unwrap();
        assert_eq!(
            classify_hook(&stop),
            HookKind::Stop {
                message: Some("pong".into()),
                transcript_path: Some("/t/cc7a37cf-9343.jsonl".into()),
            }
        );
        let perm: Value = serde_json::from_str(
            r#"{"session_id":"e237a624","hook_event_name":"Notification","message":"Claude needs your permission","notification_type":"permission_prompt"}"#,
        )
        .unwrap();
        assert_eq!(classify_hook(&perm), HookKind::PermissionPrompt);
        let idle: Value = serde_json::from_str(
            r#"{"session_id":"cc7a37cf","hook_event_name":"Notification","message":"Claude is waiting for your input","notification_type":"idle_prompt"}"#,
        )
        .unwrap();
        assert_eq!(classify_hook(&idle), HookKind::Other);
    }

    #[test]
    fn only_a_stop_with_text_is_an_idle_reply() {
        let driver = ClaudeDriver::new(Path::new("/w"), Path::new("/s"));
        let stop = |m: &str| serde_json::json!({"session_id": "s", "hook_event_name": "Stop", "last_assistant_message": m});
        assert_eq!(
            driver.idle_reply(&stop("tests pass")).as_deref(),
            Some("tests pass")
        );
        assert_eq!(driver.idle_reply(&stop("  ")), None);
        assert_eq!(
            driver.idle_reply(&serde_json::json!({"hook_event_name": "Notification", "notification_type": "permission_prompt"})),
            None
        );
    }

    #[test]
    fn hooks_match_by_stored_session_id_only() {
        let driver = ClaudeDriver::new(Path::new("/w"), Path::new("/s"));
        let session = SessionRef {
            session_id: Some("cc7a37cf-9343".into()),
            bg_id: Some("cc7a37cf".into()),
            transcript_path: None,
        };
        let ours = serde_json::json!({"session_id": "cc7a37cf-9343"});
        let theirs = serde_json::json!({"session_id": "ddc53afe-477f"});
        assert!(driver.hook_matches(&ours, &session));
        assert!(!driver.hook_matches(&theirs, &session));
        assert!(!driver.hook_matches(&ours, &SessionRef::default()));
        assert!(!driver.hook_matches(&serde_json::json!({}), &session));
    }

    const TRANSCRIPT: &str = r#"{"type":"user","timestamp":"2026-10-24T17:52:46.983Z","message":{"role":"user","content":"Reply pong"}}
{"type":"assistant","timestamp":"2026-10-24T17:52:50.078Z","message":{"role":"assistant","content":[{"type":"text","text":"pong"}]}}
{"type":"user","timestamp":"2026-10-24T17:54:24.940Z","message":{"role":"user","content":"Again"}}
{"type":"assistant","timestamp":"2026-10-24T17:54:26.000Z","message":{"role":"assistant","content":[{"type":"thinking"}]}}
{"type":"assistant","timestamp":"2026-10-24T17:54:26.500Z","isSidechain":true,"message":{"role":"assistant","content":[{"type":"text","text":"subagent"}]}}
{"type":"assistant","timestamp":"2026-10-24T17:54:27.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash"}]}}
{"type":"assistant","timestamp":"2026-10-24T17:54:27.504Z","message":{"role":"assistant","content":[{"type":"text","text":"pong again"}]}}
not json
"#;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn the_transcript_reply_is_this_turns_last_text() {
        assert_eq!(
            last_assistant_text(TRANSCRIPT, at("2026-10-24T17:54:24Z")).as_deref(),
            Some("pong again")
        );
        assert_eq!(
            last_assistant_text(TRANSCRIPT, at("2026-10-24T17:50:00Z")).as_deref(),
            Some("pong again")
        );
        // A turn that has written nothing yet is not done.
        assert_eq!(
            last_assistant_text(TRANSCRIPT, at("2026-10-24T18:00:00Z")),
            None
        );
    }

    #[test]
    fn the_permission_notice_names_the_last_tool() {
        assert_eq!(last_tool_use(TRANSCRIPT).as_deref(), Some("Bash"));
        assert_eq!(
            needs_you_notice(Some("Bash")),
            "Waiting for your approval to use Bash."
        );
        assert_eq!(
            needs_you_notice(Some("mcp__github__create_pr")),
            "Waiting for your approval."
        );
        assert_eq!(needs_you_notice(None), "Waiting for your approval.");
    }

    // -----------------------------------------------------------------------
    // `watch` against a fake `claude`
    // -----------------------------------------------------------------------

    /// A temp dir with a fake `claude` that answers `agents --json --all`
    /// with `list.json` and logs every call to `calls.log`.
    struct FakeClaude {
        dir: tempfile::TempDir,
    }

    impl FakeClaude {
        fn new(list: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let script = dir.path().join("claude");
            std::fs::write(
                &script,
                "#!/bin/sh\nd=$(dirname \"$0\")\necho \"$*\" >> \"$d/calls.log\"\ncase \"$1\" in\n  \
                 agents) cat \"$d/list.json\" ;;\n  stop) echo \"stopped $2\" ;;\n  \
                 --bg) printf 'backgrounded \\302\\267 abc12345 \\302\\267 whisrs-jarvis\\n' ;;\n  \
                 *) exit 1 ;;\nesac\n",
            )
            .unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::write(dir.path().join("list.json"), list).unwrap();
            std::fs::write(dir.path().join("transcript.jsonl"), "").unwrap();
            Self { dir }
        }

        fn agent(&self) -> Agent {
            let mut a = agent(Permissions::Auto);
            a.dir = self.dir.path().to_path_buf();
            a.command = self
                .dir
                .path()
                .join("claude")
                .to_string_lossy()
                .into_owned();
            a
        }

        fn calls(&self) -> String {
            std::fs::read_to_string(self.dir.path().join("calls.log")).unwrap_or_default()
        }

        fn session(&self) -> SessionRef {
            SessionRef {
                session_id: Some("abc12345-1111".into()),
                bg_id: Some("abc12345".into()),
                transcript_path: Some(self.dir.path().join("transcript.jsonl")),
            }
        }
    }

    fn fast_driver(poll: Duration, grace: Duration) -> ClaudeDriver {
        ClaudeDriver {
            hook_command: "true".into(),
            poll_interval: poll,
            done_grace: grace,
            use_scope: false,
        }
    }

    fn entry(pid: Option<u32>, state: &str) -> String {
        let pid = pid.map(|p| format!("\"pid\":{p},")).unwrap_or_default();
        format!(
            r#"[{{{pid}"id":"abc12345","kind":"background","sessionId":"abc12345-1111","name":"whisrs-jarvis","state":"{state}"}}]"#
        )
    }

    fn stop_event(message: &str, received: Instant) -> HookEvent {
        HookEvent {
            received,
            payload: serde_json::json!({
                "session_id": "abc12345-1111",
                "hook_event_name": "Stop",
                "last_assistant_message": message,
            }),
        }
    }

    /// The old process of a resumed session can send its Stop while the
    /// resume launches: only a Stop received after the launch ends the turn.
    #[tokio::test]
    async fn a_stop_from_before_the_launch_is_not_the_reply() {
        let fake = FakeClaude::new(&entry(Some(1), "working"));
        let driver = fast_driver(Duration::from_secs(3600), DONE_GRACE);
        let (hooks_tx, mut hooks) = mpsc::unbounded_channel();
        let (updates, _rx) = TurnUpdates::channel();
        let before = Instant::now();
        let launched = before + Duration::from_millis(5);
        hooks_tx.send(stop_event("STALE", before)).unwrap();
        hooks_tx
            .send(stop_event("fresh", launched + Duration::from_millis(1)))
            .unwrap();
        let outcome = driver
            .watch(
                &fake.agent(),
                fake.session(),
                Utc::now(),
                Some(launched),
                &mut hooks,
                &updates,
            )
            .await;
        assert_eq!(outcome, TurnOutcome::Reply("fresh".into()));
        // A turn watched again after a restart takes every event.
        assert!(is_current(&stop_event("x", before), None));
        assert!(!is_current(&stop_event("x", before), Some(launched)));
    }

    /// An exited entry keeps its last state: a killed process mid-turn
    /// (`working`, no pid) ends the turn instead of waiting forever.
    #[tokio::test]
    async fn an_exited_session_ends_the_turn() {
        let fake = FakeClaude::new(&entry(None, "working"));
        let driver = fast_driver(Duration::from_millis(20), Duration::from_millis(10));
        let (_hooks_tx, mut hooks) = mpsc::unbounded_channel();
        let (updates, _rx) = TurnUpdates::channel();
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            driver.watch(
                &fake.agent(),
                fake.session(),
                Utc::now(),
                Some(Instant::now()),
                &mut hooks,
                &updates,
            ),
        )
        .await
        .expect("an exited session must end the turn");
        match outcome {
            TurnOutcome::Failed(m) => assert!(m.contains("the session ended"), "{m}"),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    /// A vanished session ends the turn and is forgotten.
    #[tokio::test]
    async fn a_vanished_session_ends_the_turn_and_is_forgotten() {
        let fake = FakeClaude::new("[]");
        let driver = fast_driver(Duration::from_millis(20), Duration::from_millis(10));
        let (_hooks_tx, mut hooks) = mpsc::unbounded_channel();
        let (updates, mut rx) = TurnUpdates::channel();
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            driver.watch(
                &fake.agent(),
                fake.session(),
                Utc::now(),
                None,
                &mut hooks,
                &updates,
            ),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, TurnOutcome::Failed(ref m) if m.contains("no longer lists it")));
        assert_eq!(
            rx.try_recv().unwrap(),
            TurnUpdate::Session(SessionRef::default())
        );
    }

    /// `done` with nothing written since the turn started (a restart between
    /// the stop and the resume) does not wait forever: after a few polls the
    /// turn ends as interrupted.
    #[tokio::test]
    async fn done_without_text_ends_as_interrupted() {
        let fake = FakeClaude::new(&entry(Some(1), "done"));
        let driver = fast_driver(Duration::from_millis(20), Duration::from_millis(5));
        let (_hooks_tx, mut hooks) = mpsc::unbounded_channel();
        let (updates, _rx) = TurnUpdates::channel();
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            driver.watch(
                &fake.agent(),
                fake.session(),
                Utc::now(),
                None,
                &mut hooks,
                &updates,
            ),
        )
        .await
        .expect("done without text must end");
        assert_eq!(
            outcome,
            TurnOutcome::Failed(format!("Agent 'jarvis': {INTERRUPTED}"))
        );
        // One `claude agents` per poll: the first poll that said done, and
        // one more per grace that found no text.
        let polls = fake
            .calls()
            .lines()
            .filter(|l| l.starts_with("agents"))
            .count();
        assert!(polls >= DONE_POLLS_WITHOUT_TEXT as usize, "{polls} polls");
    }

    /// `done` with this turn's text in the transcript (the Stop hook never
    /// came) is the reply.
    #[tokio::test]
    async fn done_reads_the_transcript_when_the_hook_is_missing() {
        let fake = FakeClaude::new(&entry(Some(1), "done"));
        let since = Utc::now() - chrono::Duration::seconds(5);
        let line = format!(
            r#"{{"type":"assistant","timestamp":"{}","message":{{"content":[{{"type":"text","text":"from the transcript"}}]}}}}"#,
            Utc::now().to_rfc3339()
        );
        std::fs::write(fake.dir.path().join("transcript.jsonl"), line + "\n").unwrap();
        let driver = fast_driver(Duration::from_millis(20), Duration::from_millis(5));
        let (_hooks_tx, mut hooks) = mpsc::unbounded_channel();
        let (updates, _rx) = TurnUpdates::channel();
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            driver.watch(
                &fake.agent(),
                fake.session(),
                since,
                None,
                &mut hooks,
                &updates,
            ),
        )
        .await
        .unwrap();
        assert_eq!(outcome, TurnOutcome::Reply("from the transcript".into()));
    }

    /// While a `done` poll waits for the Stop hook, other hook events are
    /// still handled (the permission prompt is announced), not swallowed.
    #[tokio::test]
    async fn the_done_grace_keeps_handling_hooks() {
        let fake = FakeClaude::new(&entry(Some(1), "done"));
        let driver = fast_driver(Duration::from_millis(30), Duration::from_secs(3));
        let (hooks_tx, mut hooks) = mpsc::unbounded_channel();
        let (updates, mut rx) = TurnUpdates::channel();
        tokio::spawn(async move {
            // After the first `done` poll (at 30 ms) set the grace.
            tokio::time::sleep(Duration::from_millis(400)).await;
            let prompt = serde_json::json!({
                "session_id": "abc12345-1111",
                "hook_event_name": "Notification",
                "notification_type": "permission_prompt",
            });
            hooks_tx.send(HookEvent::now(prompt)).unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            hooks_tx
                .send(stop_event("late reply", Instant::now()))
                .unwrap();
        });
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            driver.watch(
                &fake.agent(),
                fake.session(),
                Utc::now(),
                Some(Instant::now()),
                &mut hooks,
                &updates,
            ),
        )
        .await
        .unwrap();
        assert_eq!(outcome, TurnOutcome::Reply("late reply".into()));
        let mut saw_needs_you = false;
        while let Ok(update) = rx.try_recv() {
            saw_needs_you |= matches!(update, TurnUpdate::NeedsYou(_));
        }
        assert!(saw_needs_you, "the permission prompt was swallowed");
    }

    /// A turn watched again after a restart that landed before its session
    /// id was looked up learns the id from the poll.
    #[tokio::test]
    async fn a_rewatch_by_bg_id_learns_the_session_id() {
        let fake = FakeClaude::new(&entry(None, "stopped"));
        let driver = fast_driver(Duration::from_millis(20), Duration::from_millis(5));
        let (_hooks_tx, mut hooks) = mpsc::unbounded_channel();
        let (updates, mut rx) = TurnUpdates::channel();
        let mut session = fake.session();
        session.session_id = None;
        let _ = tokio::time::timeout(
            Duration::from_secs(10),
            driver.watch(
                &fake.agent(),
                session,
                Utc::now(),
                None,
                &mut hooks,
                &updates,
            ),
        )
        .await
        .unwrap();
        match rx.try_recv().unwrap() {
            TurnUpdate::Session(s) => assert_eq!(s.session_id.as_deref(), Some("abc12345-1111")),
            other => panic!("expected the session id, got {other:?}"),
        }
    }

    /// A `--stop` that lands while `claude --bg` runs: the launch still
    /// finishes, reports its bg id, and stops the session it started.
    #[tokio::test]
    async fn a_launch_that_outlives_its_turn_stops_its_session() {
        let fake = FakeClaude::new("[]");
        let (updates, _rx) = TurnUpdates::channel();
        assert_eq!(updates.stop(), None);
        let result = launch_reported(
            &fake.agent(),
            vec!["--bg".into(), "--".into(), "hi".into()],
            SessionRef::default(),
            &updates,
            false,
        )
        .await;
        assert!(result.is_err());
        assert!(fake.calls().contains("stop abc12345"), "{}", fake.calls());
        // Not stopped: the bg id is reported and nothing is stopped.
        let fake = FakeClaude::new("[]");
        let (updates, _rx) = TurnUpdates::channel();
        let (bg_id, session) = launch_reported(
            &fake.agent(),
            vec!["--bg".into(), "--".into(), "hi".into()],
            SessionRef::default(),
            &updates,
            false,
        )
        .await
        .unwrap();
        assert_eq!(bg_id, "abc12345");
        assert_eq!(updates.stop(), Some(session));
        assert!(!fake.calls().contains("stop"), "{}", fake.calls());
    }

    /// The next turn is refused, not `claude stop`-ped, while the session
    /// works on its own.
    #[tokio::test]
    async fn a_working_session_is_not_stopped_for_a_new_turn() {
        let fake = FakeClaude::new(&entry(Some(1), "working"));
        let driver = fast_driver(POLL_INTERVAL, DONE_GRACE);
        let (updates, _rx) = TurnUpdates::channel();
        let result = driver
            .next_turn(&fake.agent(), &fake.session(), "next", &updates)
            .await;
        assert_eq!(
            result.unwrap_err(),
            format!("Agent 'jarvis': {STILL_WORKING}")
        );
        assert!(!fake.calls().contains("stop"), "{}", fake.calls());
        assert_eq!(
            driver.session_phase(&fake.agent(), &fake.session()).await,
            Ok(AgentPhase::Working)
        );
        // Done: stopped, then resumed.
        let fake = FakeClaude::new(&entry(Some(1), "done"));
        let (updates, _rx) = TurnUpdates::channel();
        let (session, _) = driver
            .next_turn(&fake.agent(), &fake.session(), "next", &updates)
            .await
            .unwrap();
        let calls = fake.calls();
        assert!(calls.contains("stop abc12345"), "{calls}");
        assert!(
            calls.contains("--bg --resume abc12345-1111 -- next"),
            "{calls}"
        );
        assert_eq!(session.bg_id.as_deref(), Some("abc12345"));
        assert_eq!(
            driver.session_phase(&fake.agent(), &fake.session()).await,
            Ok(AgentPhase::Idle)
        );
    }
}
