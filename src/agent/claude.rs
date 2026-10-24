//! The Claude Code driver: each turn is a background session (`claude --bg`).
//!
//! - First turn: `claude --bg --name whisrs-<agent> --permission-mode <m>
//!   --append-system-prompt <instruction> --settings <hooks> [args] -- <text>`.
//!   `--bg` prints `backgrounded · <bg id>`; the session id is read from
//!   `claude agents --json --all` by that bg id (`--session-id` is ignored
//!   with `--bg`, measured on 2.1.301). The appended prompt is recorded on the
//!   first request and reused on every resume, so it is passed once.
//! - Next turn: `claude stop <bg id>` while its process is alive, then
//!   `claude --bg --resume <session id> -- <text>`, which keeps the session id
//!   and the saved options (`--settings` included). Any other flag would start
//!   a copy. A resume that starts a copy anyway prints another bg id; the
//!   copy's ids replace the old ones.
//! - Turn end: the `Stop` hook (`whisrs agent-hook`, installed through
//!   `--settings` on whisrs-started sessions only) carries
//!   `last_assistant_message`. A `Notification` of type `permission_prompt`
//!   marks the agent needs-you. A poll of `claude agents --json --all` every
//!   ten seconds covers a missing hook: `done` reads the last assistant text
//!   from the transcript, a vanished, stopped or failed session is an error.
//!
//! Only ids this driver learned from sessions whisrs started are ever passed
//! to `claude`; nothing is matched by name.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::{Agent, Driver, Permissions, SessionRef, TurnIo, TurnOutcome, TurnRequest, TurnUpdate};

/// How often a running turn is polled.
pub const POLL_INTERVAL: Duration = Duration::from_secs(10);

/// `claude --bg` can start the background service first.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(90);

/// `claude agents --json --all` and `claude stop`.
const QUICK_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a `done` poll waits for the `Stop` hook before reading the
/// transcript instead.
const DONE_GRACE: Duration = Duration::from_secs(2);

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
}

impl ClaudeDriver {
    /// A driver whose sessions report to the daemon listening on `socket`
    /// through the `whisrs` binary at `whisrs_bin`.
    pub fn new(whisrs_bin: &Path, socket: &Path) -> Self {
        Self {
            hook_command: hook_command(whisrs_bin, socket),
            poll_interval: POLL_INTERVAL,
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
/// of continuing in place (measured on 2.1.301), so `permissions` and `args`
/// apply from the next new session on.
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
    /// `working`, `blocked`, `done`, `failed` or `stopped`.
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

/// Whether an environment variable is kept from the `claude` child.
///
/// - What a parent Claude Code session sets, so a daemon started from inside
///   a session does not make the agent look like one of its subprocesses.
///   `CLAUDE_CONFIG_DIR` and the provider selection (`CLAUDE_CODE_USE_*`,
///   `CLAUDE_CODE_SKIP_*`) stay.
/// - whisrs's own `WHISRS_*` variables (API key overrides): the first
///   `claude --bg` can start Claude Code's background service, which keeps
///   the environment it was started with for every session it hosts.
pub fn strips_env(name: &str) -> bool {
    match name {
        "CLAUDECODE" | "CLAUDE_PID" | "CLAUDE_EFFORT" | "CLAUDE_PROJECT_DIR" | "CLAUDE_JOB_DIR"
        | "AI_AGENT" => true,
        _ if name.starts_with("WHISRS_") => true,
        _ => {
            name.starts_with("CLAUDE_CODE_")
                && !name.starts_with("CLAUDE_CODE_USE_")
                && !name.starts_with("CLAUDE_CODE_SKIP_")
        }
    }
}

/// The notification and error text for a failed launch.
pub fn launch_failure(agent: &Agent, stdout: &str, stderr: &str) -> String {
    if stderr.contains("Workspace not trusted") || stdout.contains("Workspace not trusted") {
        return format!(
            "Agent '{}': Open {} in Claude Code once and accept the trust prompt.",
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

/// The error for a binary that cannot be started.
pub fn spawn_failure(agent: &Agent, error: &std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
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
/// Only our own session's file is ever opened.
fn find_transcript(session_id: &str) -> Option<PathBuf> {
    let projects = claude_config_dir()?.join("projects");
    let file = format!("{session_id}.jsonl");
    std::fs::read_dir(projects)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path().join(&file))
        .find(|p| p.is_file())
}

// ---------------------------------------------------------------------------
// Process plumbing
// ---------------------------------------------------------------------------

/// Captured output of a finished `claude` invocation.
struct Output {
    success: bool,
    stdout: String,
    stderr: String,
}

/// Run the agent's `claude` with `args` in its dir, stdin closed, the parent
/// session's variables removed, killed if dropped or past `timeout`.
async fn run_claude(agent: &Agent, args: &[String], timeout: Duration) -> Result<Output, String> {
    let mut cmd = tokio::process::Command::new(&agent.command);
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
    let child = cmd.spawn().map_err(|e| spawn_failure(agent, &e))?;
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => Ok(Output {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }),
        Ok(Err(e)) => Err(format!(
            "Agent '{}': `{}` failed: {e}",
            agent.name, agent.command
        )),
        Err(_) => Err(format!(
            "Agent '{}': `{} {}` did not finish in {} seconds",
            agent.name,
            agent.command,
            args.first().map(String::as_str).unwrap_or(""),
            timeout.as_secs()
        )),
    }
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

impl ClaudeDriver {
    /// Launch `args` and return the bg id it printed.
    async fn launch(&self, agent: &Agent, args: &[String]) -> Result<String, String> {
        let out = run_claude(agent, args, LAUNCH_TIMEOUT).await?;
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

    /// The first turn of a new session.
    async fn first_turn(&self, agent: &Agent, text: &str) -> Result<SessionRef, String> {
        let settings = hook_settings(&self.hook_command);
        let bg_id = self
            .launch(agent, &first_turn_args(agent, &settings, text))
            .await?;
        let listed = learn_session(agent, &bg_id).await?;
        info!(
            "agent '{}': started session {} (bg {bg_id})",
            agent.name,
            listed.session_id.as_deref().unwrap_or("?")
        );
        Ok(SessionRef {
            session_id: listed.session_id,
            bg_id: Some(bg_id),
            transcript_path: None,
        })
    }

    /// A later turn: stop the old process if it is alive, then resume.
    async fn next_turn(
        &self,
        agent: &Agent,
        session: &SessionRef,
        session_id: &str,
        text: &str,
    ) -> Result<SessionRef, String> {
        if let Some(bg_id) = &session.bg_id {
            let alive = match list_sessions(agent).await {
                Ok(list) => match find_session(&list, bg_id) {
                    Some(listed) => listed.alive,
                    None => {
                        // Removed (`claude rm`): a resume would come back
                        // without its saved options, hooks included.
                        info!(
                            "agent '{}': session {session_id} is no longer listed; starting a \
                             new one",
                            agent.name
                        );
                        return self.first_turn(agent, text).await;
                    }
                },
                // Cannot tell: stopping a stopped session is harmless.
                Err(_) => true,
            };
            if alive {
                self.stop_bg(agent, bg_id).await;
            }
        }
        let bg_id = self.launch(agent, &resume_args(session_id, text)).await?;
        if session.bg_id.as_deref() == Some(bg_id.as_str()) {
            return Ok(session.clone());
        }
        // A copy: Claude Code could not continue in place.
        let listed = learn_session(agent, &bg_id).await?;
        warn!(
            "agent '{}': resume started a copy: session {} (bg {bg_id}) replaces {session_id}",
            agent.name,
            listed.session_id.as_deref().unwrap_or("?")
        );
        Ok(SessionRef {
            session_id: listed.session_id,
            bg_id: Some(bg_id),
            transcript_path: None,
        })
    }

    async fn stop_bg(&self, agent: &Agent, bg_id: &str) {
        let args = ["stop".to_string(), bg_id.to_string()];
        match run_claude(agent, &args, QUICK_TIMEOUT).await {
            Ok(out) if out.success => debug!("agent '{}': stopped bg {bg_id}", agent.name),
            Ok(out) => warn!(
                "agent '{}': `claude stop {bg_id}` failed: {}",
                agent.name,
                out.stderr.trim()
            ),
            Err(e) => warn!("{e}"),
        }
    }

    /// Wait for the turn of `session` to end.
    async fn watch(
        &self,
        agent: &Agent,
        mut session: SessionRef,
        since: DateTime<Utc>,
        hooks: &mut mpsc::UnboundedReceiver<Value>,
        updates: &mpsc::UnboundedSender<TurnUpdate>,
    ) -> TurnOutcome {
        let Some(bg_id) = session.bg_id.clone() else {
            return TurnOutcome::Failed(format!(
                "Agent '{}': the turn was interrupted before its session started",
                agent.name
            ));
        };
        let mut poll = tokio::time::interval(self.poll_interval);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick is immediate; the hook usually comes first.
        poll.tick().await;
        let mut needs_you = false;
        let mut hooks_open = true;
        loop {
            tokio::select! {
                payload = hooks.recv(), if hooks_open => {
                    let Some(payload) = payload else {
                        hooks_open = false;
                        continue;
                    };
                    match classify_hook(&payload) {
                        HookKind::Stop { message, transcript_path } => {
                            if let Some(path) = transcript_path {
                                if session.transcript_path.as_ref() != Some(&path) {
                                    session.transcript_path = Some(path);
                                    let _ = updates.send(TurnUpdate::Session(session.clone()));
                                }
                            }
                            let text = match message {
                                Some(m) if !m.trim().is_empty() => m,
                                _ => self.transcript_reply(&session, since).unwrap_or_default(),
                            };
                            return TurnOutcome::Reply(text);
                        }
                        HookKind::PermissionPrompt => {
                            if !needs_you {
                                needs_you = true;
                                let _ = updates.send(TurnUpdate::NeedsYou(self.notice(&session)));
                            }
                        }
                        HookKind::Other => {}
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
                    let Some(listed) = find_session(&list, &bg_id) else {
                        // Forget it: the next turn starts a new session.
                        let _ = updates.send(TurnUpdate::Session(SessionRef::default()));
                        return TurnOutcome::Failed(format!(
                            "Agent '{}': the session ended (Claude Code no longer lists it). \
                             The next turn starts a new one.",
                            agent.name
                        ));
                    };
                    match listed.state.as_deref() {
                        Some("blocked") => {
                            if !needs_you {
                                needs_you = true;
                                let notice = match listed.waiting_for.as_deref() {
                                    Some("permission prompt") | None => self.notice(&session),
                                    Some(_) => "The agent needs you.".to_string(),
                                };
                                let _ = updates.send(TurnUpdate::NeedsYou(notice));
                            }
                        }
                        Some("working") => {
                            if needs_you {
                                needs_you = false;
                                let _ = updates.send(TurnUpdate::Working);
                            }
                        }
                        Some("done") => {
                            // Give the Stop hook a moment: it carries the
                            // message without reading the transcript.
                            if let Ok(Some(payload)) =
                                tokio::time::timeout(DONE_GRACE, hooks.recv()).await
                            {
                                if let HookKind::Stop { message: Some(m), .. } =
                                    classify_hook(&payload)
                                {
                                    if !m.trim().is_empty() {
                                        return TurnOutcome::Reply(m);
                                    }
                                }
                            }
                            // `done` before this turn wrote anything is the
                            // previous turn's state; keep waiting.
                            if let Some(text) = self.transcript_reply(&session, since) {
                                info!(
                                    "agent '{}': turn done without a Stop hook; read the transcript",
                                    agent.name
                                );
                                return TurnOutcome::Reply(text);
                            }
                        }
                        Some("failed") => {
                            return TurnOutcome::Failed(format!(
                                "Agent '{}': the session failed. Open it with `claude attach \
                                 {bg_id}` to see why.",
                                agent.name
                            ));
                        }
                        Some("stopped") => {
                            return TurnOutcome::Failed(format!(
                                "Agent '{}': the session ended before replying.",
                                agent.name
                            ));
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// The last assistant text of this turn, from the transcript.
    fn transcript_reply(&self, session: &SessionRef, since: DateTime<Utc>) -> Option<String> {
        let path = session
            .transcript_path
            .clone()
            .or_else(|| session.session_id.as_deref().and_then(find_transcript))?;
        let contents = std::fs::read_to_string(path).ok()?;
        last_assistant_text(&contents, since)
    }

    /// The needs-you notice, naming the tool when the transcript shows it.
    fn notice(&self, session: &SessionRef) -> String {
        let tool = session
            .transcript_path
            .clone()
            .or_else(|| session.session_id.as_deref().and_then(find_transcript))
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| last_tool_use(&t));
        needs_you_notice(tool.as_deref())
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
        let launched = match session.session_id.as_deref() {
            Some(session_id) => {
                self.next_turn(agent, &session, session_id, &request.text)
                    .await
            }
            None => self.first_turn(agent, &request.text).await,
        };
        let session = match launched {
            Ok(s) => s,
            Err(message) => return TurnOutcome::Failed(message),
        };
        let _ = updates.send(TurnUpdate::Session(session.clone()));
        self.watch(agent, session, request.started_at, &mut hooks, &updates)
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
        self.watch(agent, session, request.started_at, &mut hooks, &updates)
            .await
    }

    async fn stop(&self, agent: &Agent, session: &SessionRef) -> Result<(), String> {
        let Some(bg_id) = &session.bg_id else {
            return Ok(());
        };
        let args = ["stop".to_string(), bg_id.clone()];
        let out = run_claude(agent, &args, QUICK_TIMEOUT).await?;
        if out.success {
            Ok(())
        } else {
            Err(format!(
                "`claude stop {bg_id}` failed: {}",
                out.stderr.trim()
            ))
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Harness, DEFAULT_REPLY_INSTRUCTION};

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

    #[test]
    fn the_list_parser_skips_a_preface() {
        let out = format!("Starting background service\u{2026}\n{LIST}\n");
        assert_eq!(parse_agent_list(&out).unwrap().len(), 4);
        assert!(parse_agent_list("no json").is_err());
    }

    #[test]
    fn a_parent_sessions_variables_are_stripped() {
        for name in [
            "CLAUDECODE",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDE_CODE_MESSAGING_TOKEN",
            "CLAUDE_CODE_ENTRYPOINT",
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
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_SKIP_VERTEX_AUTH",
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
            "Agent 'jarvis': Open /proj in Claude Code once and accept the trust prompt."
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
        let msg = spawn_failure(&a, &e);
        assert!(msg.contains("`claude` was not found"), "{msg}");
        assert!(msg.contains("set command in [[agents]]"), "{msg}");
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
}
