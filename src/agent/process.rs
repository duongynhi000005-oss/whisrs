//! The process-per-turn driver: each turn is one harness process that prints
//! JSON lines and exits (OpenCode's `opencode run`, Codex's `codex exec`). A
//! harness module says how a turn starts and what a line of its output means
//! ([`TurnHarness`]); [`ProcessDriver`] does the rest, the same for every such
//! harness:
//!
//! - **Spawn:** the agent's `command` with the harness's arguments, in the
//!   agent's dir, with the daemon's environment minus what a parent session
//!   sets for the processes it runs (Claude Code's,
//!   [`super::claude::strips_env`], and the harness's own,
//!   [`TurnHarness::strips_env`]) and with `PWD` set to the dir: a cwd alone
//!   leaves the daemon's `PWD`, and OpenCode resolves its directory from
//!   `PWD`. In a process group of its own. Stdin is the prompt, then closed,
//!   or `/dev/null`: OpenCode and Codex read an open stdin until it ends.
//! - **The daemon's end ends the turn:** the child is spawned from a thread
//!   that waits for it, with `PR_SET_PDEATHSIG` = SIGINT, so it is
//!   interrupted when the daemon exits or dies, as the cgroup kill of a
//!   systemd unit ends it under a service. A turn never outlives its daemon
//!   (it would run unwatched, and the next turn would run beside it), and a
//!   turn watched again after a restart ends at once ([`cut_off`]).
//! - **Output:** stdout line by line ([`TurnHarness::events`]): the session
//!   id it names is reported when it is new ([`TurnUpdate::Session`]), a tool
//!   call is an activity ([`activity_for_tool`]) and releases the assistant
//!   text before it as an interim ([`InterimTracker`]), and the text of the
//!   last message is the reply. Stderr is kept for the error, which the
//!   harness may recognize ([`TurnHarness::stderr_failure`]).
//! - **Stop:** SIGINT to the group, then SIGTERM, then SIGKILL, each sent
//!   only while something of the group is left, after the previous one's
//!   grace ([`STOP_GRACES`]). A turn dropped while its process runs (its task
//!   aborted) is stopped the same way, and `--stop` waits for that stop.
//!
//! Between turns no process runs: a session never works on its own, waits
//! for a prompt or sends a hook.

use std::collections::HashMap;
use std::io::Write as _;
use std::os::unix::process::CommandExt as _;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::{oneshot, watch, OnceCell};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use super::progress::{activity_for_tool, InterimTracker};
use super::{
    claude, Agent, AgentPhase, Driver, IdleEvent, SessionRef, TurnIo, TurnOutcome, TurnRequest,
    TurnUpdate, TurnUpdates,
};

/// How long each stop signal gets: SIGINT (the harness interrupts its turn
/// and exits; OpenCode in 0.2 s, measured), then SIGTERM, and after SIGKILL
/// how long the exit may take to be seen.
pub const STOP_GRACES: [Duration; 3] = [
    Duration::from_secs(3),
    Duration::from_secs(2),
    Duration::from_secs(2),
];

/// How often a stop checks whether the process group is gone.
const STOP_POLL: Duration = Duration::from_millis(50);

/// After the process exited, how long its output is still read: a process
/// it left behind can hold its stdout open.
const DRAIN_AFTER_EXIT: Duration = Duration::from_secs(2);

/// The most stderr lines kept for an error.
const STDERR_LINES: usize = 40;

/// The longest output line logged, in characters.
const LOG_LINE_MAX: usize = 300;

/// The longest harness error quoted in a notification, in characters.
const ERROR_MAX: usize = 300;

// ---------------------------------------------------------------------------
// What a harness module provides
// ---------------------------------------------------------------------------

/// How one turn's process starts. The program is the agent's `command`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// The arguments after `command`.
    pub args: Vec<String>,
    /// Written to stdin, which is then closed. `None`: stdin is `/dev/null`.
    pub stdin: Option<String>,
    /// Set in the child's environment, after the stripped variables are
    /// removed.
    pub env: Vec<(String, String)>,
}

/// What a line of a harness's output says about the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    /// The id of the session the turn runs in.
    Session(String),
    /// Assistant text of the message `message` (the harness's id for it).
    /// The text parts of one message join, and the turn's last message is
    /// its reply.
    Text { message: String, text: String },
    /// The agent used a tool, by the harness's name for it.
    Tool(String),
    /// The turn failed: the harness's message.
    Error(String),
}

/// A harness whose turn is one process. Every method is required: there is
/// no behavior to inherit by leaving one out.
pub trait TurnHarness: Send + Sync + 'static {
    /// The harness's name in messages ("OpenCode").
    fn label(&self) -> &'static str;

    /// How a turn starts. `session` is the agent's session, empty for its
    /// first turn (or after `--new`). An error is actionable and fails the
    /// turn as it is.
    fn launch(
        &self,
        agent: &Agent,
        session: &SessionRef,
        request: &TurnRequest,
    ) -> Result<Launch, String>;

    /// Whether the daemon's environment variable `name` is removed from the
    /// child besides those every driver removes
    /// ([`super::claude::strips_env`]): what a parent session of this harness
    /// sets for the processes it runs.
    fn strips_env(&self, name: &str) -> bool;

    /// The events of one stdout line, in order. None for a line that says
    /// nothing about the turn, or is not the harness's JSON.
    fn events(&self, line: &str) -> Vec<StreamEvent>;

    /// The failure of a turn that printed no error event and no reply and
    /// exited unsuccessfully, from its last stderr lines, when the harness
    /// recognizes them (a session it no longer has); `None` leaves it to
    /// the line that mentions an error ([`stderr_detail`]).
    fn stderr_failure(&self, agent: &Agent, stderr: &[String]) -> Option<String>;
}

// ---------------------------------------------------------------------------
// Pure parts
// ---------------------------------------------------------------------------

/// What a turn's output said so far, and the updates it is worth.
#[derive(Debug)]
pub struct StreamState {
    /// The session id known: the agent's, then the newest the output named.
    session_id: Option<String>,
    tracker: InterimTracker,
    /// The last activity sent, so a run of the same tool sends it once.
    activity: Option<String>,
    /// Assistant texts with their message ids, in order.
    texts: Vec<(String, String)>,
    /// The last error the harness reported.
    error: Option<String>,
}

impl StreamState {
    /// The state of a turn in `session_id` (`None` for a new session).
    pub fn new(session_id: Option<String>) -> Self {
        Self {
            session_id,
            tracker: InterimTracker::new(),
            activity: None,
            texts: Vec::new(),
            error: None,
        }
    }

    /// Take one event, reporting what the runtime needs to `updates`.
    pub fn event(&mut self, event: StreamEvent, updates: &TurnUpdates) {
        match event {
            StreamEvent::Session(id) => {
                if self.session_id.as_deref() != Some(id.as_str()) {
                    self.session_id = Some(id.clone());
                    updates.send(TurnUpdate::Session(SessionRef {
                        session_id: Some(id),
                        bg_id: None,
                        transcript_path: None,
                    }));
                }
            }
            StreamEvent::Text { message, text } => {
                self.tracker.on_text(&text);
                self.texts.push((message, text));
            }
            StreamEvent::Tool(name) => {
                let activity = activity_for_tool(&name);
                if self.activity.as_ref() != Some(&activity) {
                    self.activity = Some(activity.clone());
                    updates.send(TurnUpdate::Activity(activity));
                }
                if let Some(raw) = self.tracker.on_tool() {
                    updates.send(TurnUpdate::Interim(raw));
                }
            }
            StreamEvent::Error(message) => self.error = Some(message),
        }
    }

    /// The turn's final message: the text parts of its last message, raw.
    /// `None` when it wrote none.
    pub fn reply(&self) -> Option<String> {
        let (last, _) = self.texts.last()?;
        let text = self
            .texts
            .iter()
            .filter(|(message, _)| message == last)
            .map(|(_, text)| text.trim())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        (!text.is_empty()).then_some(text)
    }

    /// The turn ended: what the tracker holds is the final message, never
    /// an interim.
    fn end(&mut self) {
        self.tracker.end();
    }
}

/// How a turn's process ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    Status(ExitStatus),
    /// Its exit could not be read.
    Lost(String),
}

impl Exit {
    fn success(&self) -> bool {
        matches!(self, Exit::Status(status) if status.success())
    }
}

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Exit::Status(status) => write!(f, "{status}"),
            Exit::Lost(e) => write!(f, "exit unknown: {e}"),
        }
    }
}

/// How the turn ended, from what its output said and how its process
/// exited. An error the harness reported fails it; otherwise the last
/// message is the reply (a non-zero exit after one is only logged); a turn
/// with neither fails with what stderr said, as the harness reads it
/// ([`TurnHarness::stderr_failure`]) or else generically.
pub fn outcome(
    agent: &Agent,
    harness: &dyn TurnHarness,
    state: &StreamState,
    stderr: &[String],
    exit: &Exit,
) -> TurnOutcome {
    let label = harness.label();
    if let Some(error) = &state.error {
        return TurnOutcome::Failed(format!(
            "Agent '{}': {label} failed: {}",
            agent.name,
            sentence(error)
        ));
    }
    if let Some(reply) = state.reply() {
        if !exit.success() {
            warn!(
                "agent '{}': {label} replied, then exited with {exit}",
                agent.name
            );
        }
        return TurnOutcome::Reply(reply);
    }
    if exit.success() {
        return TurnOutcome::Failed(format!(
            "Agent '{}': {label} ended the turn without a reply.",
            agent.name
        ));
    }
    if let Some(message) = harness.stderr_failure(agent, stderr) {
        return TurnOutcome::Failed(message);
    }
    let detail = stderr_detail(stderr)
        .map(|line| sentence(&line))
        .unwrap_or_else(|| format!("it exited with {exit}."));
    TurnOutcome::Failed(format!("Agent '{}': {label} failed: {detail}", agent.name))
}

/// The stderr line that best says what went wrong: the first that mentions
/// an error, else the last one. Color codes removed; `None` without any.
pub fn stderr_detail(lines: &[String]) -> Option<String> {
    let lines: Vec<String> = lines
        .iter()
        .map(|l| strip_ansi(l).trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    lines
        .iter()
        .find(|l| l.to_ascii_lowercase().contains("error"))
        .or(lines.last())
        .cloned()
}

/// `text` on one line, capped at [`ERROR_MAX`] characters, ending in
/// punctuation.
pub fn sentence(text: &str) -> String {
    let mut line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > ERROR_MAX {
        line = line.chars().take(ERROR_MAX).collect::<String>() + "...";
    }
    if !line.ends_with(['.', '!', '?']) {
        line.push('.');
    }
    line
}

/// `text` without ANSI escape sequences (colors, cursor moves).
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            // Parameters and intermediates, up to the final byte.
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            chars.next();
        }
    }
    out
}

/// The error of a turn watched again after a restart: its process ended
/// with the old daemon (see the module doc), so it never replied.
pub fn cut_off(agent: &Agent, label: &str) -> String {
    format!(
        "Agent '{}': whisrs restarted during that turn, which stopped it before {label} replied. \
         The session keeps what it did so far.",
        agent.name
    )
}

/// The error for a process that cannot be started. Spawning in a missing
/// working directory fails with "not found" too, so the dir is checked
/// first (`dir_exists`).
pub fn spawn_failure(
    agent: &Agent,
    label: &str,
    error: &std::io::Error,
    dir_exists: bool,
) -> String {
    if !dir_exists {
        format!(
            "Agent '{}': its dir {} does not exist.",
            agent.name,
            agent.dir.display()
        )
    } else if error.kind() == std::io::ErrorKind::NotFound {
        format!(
            "Agent '{}': `{}` was not found. Install {label}, or set command in [[agents]] to its \
             full path (a systemd user service may not have ~/.local/bin on PATH).",
            agent.name, agent.command
        )
    } else {
        format!(
            "Agent '{}': cannot run `{}`: {error}",
            agent.name, agent.command
        )
    }
}

/// `N` random bytes from the kernel (`getrandom`), for ids a harness
/// takes from whisrs.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    let mut filled = 0;
    while filled < N {
        // SAFETY: writes at most `N - filled` bytes into `bytes[filled..]`.
        let n = unsafe {
            libc::getrandom(
                bytes[filled..].as_mut_ptr().cast::<libc::c_void>(),
                N - filled,
                0,
            )
        };
        if n > 0 {
            filled += n as usize;
        } else if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            // No kernel randomness (never on a supported kernel): the clock
            // still keeps ids apart.
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            for (i, b) in bytes[filled..].iter_mut().enumerate() {
                *b = (nanos >> ((i % 16) * 8)) as u8 ^ (i as u8).wrapping_mul(151);
            }
            break;
        }
    }
    bytes
}

// ---------------------------------------------------------------------------
// The running process
// ---------------------------------------------------------------------------

/// Whether any process of the group `pgid` is left (a zombie counts).
fn group_alive(pgid: u32) -> bool {
    let Some(pgid) = group_target(pgid) else {
        return false;
    };
    // SAFETY: signal 0 sends nothing; it only checks the group exists.
    let rc = unsafe { libc::kill(pgid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Send `signal` to the group `pgid`.
fn signal_group(pgid: u32, signal: libc::c_int) {
    if let Some(pgid) = group_target(pgid) {
        // SAFETY: a plain kill(2) of our child's own process group.
        unsafe { libc::kill(pgid, signal) };
    }
}

/// The negative pid that names the group `pgid` for kill(2). Never 0 or
/// 1: `kill(0)` would signal the daemon's own group and `kill(-1)` every
/// process it may signal.
fn group_target(pgid: u32) -> Option<libc::pid_t> {
    libc::pid_t::try_from(pgid)
        .ok()
        .filter(|&p| p > 1)
        .map(|p| -p)
}

/// A turn's running process, kept so `--stop` can end it.
#[derive(Debug)]
struct TurnProcess {
    /// Its pid, which is also its process group's id.
    pid: u32,
    /// Its exit, once the thread that waits for it saw it.
    exit: watch::Receiver<Option<Exit>>,
    /// The stop, run once however many ask for it.
    stopped: OnceCell<Result<(), String>>,
}

impl TurnProcess {
    fn exited(&self) -> bool {
        self.exit.borrow().is_some()
    }

    /// The process exited and nothing of its group is left.
    fn gone(&self) -> bool {
        self.exited() && !group_alive(self.pid)
    }

    /// Stop it ([`TurnProcess::escalate`]), once: a second caller waits for
    /// the first one's stop.
    async fn stop(&self, graces: &[Duration; 3]) -> Result<(), String> {
        self.stopped
            .get_or_init(|| self.escalate(graces))
            .await
            .clone()
    }

    /// SIGINT, SIGTERM, SIGKILL to the group, each while something of it is
    /// left, after the previous one's grace.
    async fn escalate(&self, graces: &[Duration; 3]) -> Result<(), String> {
        for (signal, grace) in [libc::SIGINT, libc::SIGTERM, libc::SIGKILL]
            .into_iter()
            .zip(graces)
        {
            if self.gone() {
                return Ok(());
            }
            debug!("turn process {}: signal {signal} to its group", self.pid);
            signal_group(self.pid, signal);
            if self.wait_gone(*grace).await {
                return Ok(());
            }
        }
        Err(format!("its process {} did not exit", self.pid))
    }

    /// Wait up to `within` for [`TurnProcess::gone`].
    async fn wait_gone(&self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        loop {
            if self.gone() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(STOP_POLL).await;
        }
    }
}

/// The processes of running turns, by agent name.
#[derive(Debug, Default)]
struct TurnProcesses {
    running: Mutex<HashMap<String, Arc<TurnProcess>>>,
}

impl TurnProcesses {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<TurnProcess>>> {
        self.running.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn get(&self, agent: &str) -> Option<Arc<TurnProcess>> {
        self.map().get(agent).cloned()
    }

    fn insert(&self, agent: &str, process: Arc<TurnProcess>) {
        self.map().insert(agent.to_string(), process);
    }

    /// Forget `process` as `agent`'s, unless another one replaced it.
    fn remove(&self, agent: &str, process: &Arc<TurnProcess>) {
        let mut map = self.map();
        if map.get(agent).is_some_and(|p| Arc::ptr_eq(p, process)) {
            map.remove(agent);
        }
    }
}

/// Keeps a turn's process registered while the turn runs. Dropped with the
/// turn: a process that still runs then (the turn's task was aborted) is
/// stopped on a task of its own, and stays registered until it is gone, so
/// a `--stop` and the next turn wait for it.
struct Registered {
    processes: Arc<TurnProcesses>,
    agent: String,
    process: Arc<TurnProcess>,
    graces: [Duration; 3],
}

impl Drop for Registered {
    fn drop(&mut self) {
        if self.process.exited() {
            self.processes.remove(&self.agent, &self.process);
            return;
        }
        let processes = Arc::clone(&self.processes);
        let process = Arc::clone(&self.process);
        let agent = self.agent.clone();
        let graces = self.graces;
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    if let Err(e) = process.stop(&graces).await {
                        warn!("agent '{agent}': stopping a dropped turn: {e}");
                    }
                    processes.remove(&agent, &process);
                });
            }
            // No runtime left to wait on: interrupt it, as the daemon's exit
            // does (PR_SET_PDEATHSIG).
            Err(_) => signal_group(process.pid, libc::SIGINT),
        }
    }
}

/// A spawned turn process.
struct Spawned {
    /// As registered for a stop.
    process: Arc<TurnProcess>,
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    exit: watch::Receiver<Option<Exit>>,
}

/// In the child, between fork and exec: get SIGINT when the thread that
/// spawned it ends (it waits for the child, so only with the daemon), and
/// fail if the daemon is already gone. Only async-signal-safe calls.
fn interrupt_with_parent(parent: u32) -> std::io::Result<()> {
    // SAFETY: prctl(PR_SET_PDEATHSIG) takes a signal number and changes
    // only this process.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGINT) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: getppid has no preconditions.
    if unsafe { libc::getppid() } as u32 != parent {
        return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
    }
    Ok(())
}

/// Spawn `launch` for `agent` (see the module doc) from a thread that
/// registers the child in `processes` (before the turn hears of it, so a
/// turn dropped meanwhile leaves it to a stop), writes the prompt, waits
/// for the child and reports its exit.
async fn spawn(
    agent: &Agent,
    launch: Launch,
    strips: impl Fn(&str) -> bool,
    processes: &Arc<TurnProcesses>,
) -> std::io::Result<Spawned> {
    let mut cmd = std::process::Command::new(&agent.command);
    cmd.args(&launch.args)
        .current_dir(&agent.dir)
        .stdin(if launch.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    for (name, _) in std::env::vars_os() {
        if let Some(name) = name.to_str() {
            if strips(name) {
                cmd.env_remove(name);
            }
        }
    }
    cmd.env("PWD", &agent.dir);
    for (name, value) in &launch.env {
        cmd.env(name, value);
    }
    let parent = std::process::id();
    // SAFETY: the closure only makes async-signal-safe calls.
    unsafe {
        cmd.pre_exec(move || interrupt_with_parent(parent));
    }
    type Pipes = (
        Arc<TurnProcess>,
        Option<std::process::ChildStdout>,
        Option<std::process::ChildStderr>,
    );
    let (ready_tx, ready_rx) = oneshot::channel::<std::io::Result<Pipes>>();
    let (exit_tx, exit_rx) = watch::channel(None);
    let prompt = launch.stdin;
    let name = agent.name.clone();
    let registry = Arc::clone(processes);
    let exit_seen = exit_rx.clone();
    std::thread::Builder::new()
        .name(format!("agent-{name}"))
        .spawn(move || {
            let mut child = match cmd.spawn() {
                Ok(child) => child,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let pid = child.id();
            let process = Arc::new(TurnProcess {
                pid,
                exit: exit_seen,
                stopped: OnceCell::new(),
            });
            registry.insert(&name, Arc::clone(&process));
            let stdin = child.stdin.take();
            let pipes = (process, child.stdout.take(), child.stderr.take());
            if ready_tx.send(Ok(pipes)).is_err() {
                // The turn was dropped while it spawned: nobody reads it.
                signal_group(pid, libc::SIGINT);
            }
            if let (Some(mut stdin), Some(prompt)) = (stdin, prompt) {
                if let Err(e) = stdin.write_all(prompt.as_bytes()) {
                    debug!("turn process {pid}: the prompt was not written: {e}");
                }
            }
            let exit = match child.wait() {
                Ok(status) => Exit::Status(status),
                Err(e) => Exit::Lost(e.to_string()),
            };
            let _ = exit_tx.send(Some(exit));
        })?;
    let (process, stdout, stderr) = ready_rx
        .await
        .map_err(|_| std::io::Error::other("the thread that spawns the turn ended"))??;
    let missing = || std::io::Error::other("the turn process has no output pipe");
    Ok(Spawned {
        process,
        stdout: tokio::process::ChildStdout::from_std(stdout.ok_or_else(missing)?)?,
        stderr: tokio::process::ChildStderr::from_std(stderr.ok_or_else(missing)?)?,
        exit: exit_rx,
    })
}

/// One line at most [`LOG_LINE_MAX`] characters long, for the log.
fn for_log(line: &str) -> String {
    let line = line.trim_end();
    if line.chars().count() > LOG_LINE_MAX {
        line.chars().take(LOG_LINE_MAX).collect::<String>() + "..."
    } else {
        line.to_string()
    }
}

/// What reading a turn's process gave.
struct Read {
    state: StreamState,
    stderr: Vec<String>,
    exit: Exit,
}

/// Read the turn's stdout and stderr until both end and the process
/// exited, or [`DRAIN_AFTER_EXIT`] after it exited.
async fn read_turn(
    harness: &dyn TurnHarness,
    agent: &str,
    stdout: impl AsyncRead + Unpin,
    stderr: impl AsyncRead + Unpin,
    mut exit: watch::Receiver<Option<Exit>>,
    state: StreamState,
    updates: &TurnUpdates,
) -> Read {
    let mut state = state;
    let mut stdout = BufReader::new(stdout);
    let mut stderr = BufReader::new(stderr);
    let (mut out_line, mut err_line) = (Vec::new(), Vec::new());
    let (mut out_done, mut err_done) = (false, false);
    let mut errors: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut drain_until: Option<Instant> = exit
        .borrow()
        .is_some()
        .then(|| Instant::now() + DRAIN_AFTER_EXIT);
    loop {
        if out_done && err_done && drain_until.is_some() {
            break;
        }
        let drain = drain_until.unwrap_or_else(Instant::now);
        tokio::select! {
            read = stdout.read_until(b'\n', &mut out_line), if !out_done => {
                match read {
                    Ok(0) | Err(_) => out_done = true,
                    Ok(_) => {
                        let line = String::from_utf8_lossy(&out_line).into_owned();
                        out_line.clear();
                        debug!("agent '{agent}': {}", for_log(&line));
                        for event in harness.events(&line) {
                            state.event(event, updates);
                        }
                    }
                }
            }
            read = stderr.read_until(b'\n', &mut err_line), if !err_done => {
                match read {
                    Ok(0) | Err(_) => err_done = true,
                    Ok(_) => {
                        let line = String::from_utf8_lossy(&err_line).into_owned();
                        err_line.clear();
                        debug!("agent '{agent}': stderr: {}", for_log(&strip_ansi(&line)));
                        if errors.len() == STDERR_LINES {
                            errors.pop_front();
                        }
                        errors.push_back(line.trim_end().to_string());
                    }
                }
            }
            // Ok once the exit is sent, Err when the thread ended without one.
            _ = exit.changed(), if drain_until.is_none() => {
                drain_until = Some(Instant::now() + DRAIN_AFTER_EXIT);
            }
            _ = tokio::time::sleep_until(drain), if drain_until.is_some() => break,
        }
    }
    state.end();
    let exit = exit
        .borrow()
        .clone()
        .unwrap_or_else(|| Exit::Lost("the thread that waits for it ended".to_string()));
    Read {
        state,
        stderr: errors.into(),
        exit,
    }
}

// ---------------------------------------------------------------------------
// The driver
// ---------------------------------------------------------------------------

/// The [`Driver`] of a harness whose turn is one process.
pub struct ProcessDriver<H> {
    harness: H,
    processes: Arc<TurnProcesses>,
    graces: [Duration; 3],
}

impl<H: TurnHarness> ProcessDriver<H> {
    pub fn new(harness: H) -> Self {
        Self::with_graces(harness, STOP_GRACES)
    }

    /// [`ProcessDriver::new`] with the stop graces injected, for tests.
    fn with_graces(harness: H, graces: [Duration; 3]) -> Self {
        Self {
            harness,
            processes: Arc::new(TurnProcesses::default()),
            graces,
        }
    }

    /// Whether the daemon's variable `name` is removed from the child.
    fn strips_env(&self, name: &str) -> bool {
        claude::strips_env(name) || self.harness.strips_env(name)
    }

    /// Wait until a process of `agent` left from a stopped or dropped turn
    /// is gone: never two of one session at once.
    async fn wait_previous(&self, agent: &Agent) {
        if let Some(previous) = self.processes.get(&agent.name) {
            debug!(
                "agent '{}': waiting for the last turn's process to stop",
                agent.name
            );
            if let Err(e) = previous.stop(&self.graces).await {
                warn!("agent '{}': {}: {e}", agent.name, self.harness.label());
            }
            self.processes.remove(&agent.name, &previous);
        }
    }
}

#[async_trait]
impl<H: TurnHarness> Driver for ProcessDriver<H> {
    async fn run_turn(
        &self,
        agent: &Agent,
        session: SessionRef,
        request: TurnRequest,
        io: TurnIo,
    ) -> TurnOutcome {
        // No hooks: nothing is ever routed to a process-per-turn session.
        let TurnIo { hooks: _, updates } = io;
        let label = self.harness.label();
        if !agent.dir.is_dir() {
            return TurnOutcome::Failed(format!(
                "Agent '{}': its dir {} no longer exists.",
                agent.name,
                agent.dir.display()
            ));
        }
        self.wait_previous(agent).await;
        let launch = match self.harness.launch(agent, &session, &request) {
            Ok(launch) => launch,
            Err(message) => return TurnOutcome::Failed(message),
        };
        debug!(
            "agent '{}': {} {}",
            agent.name,
            agent.command,
            launch.args.join(" ")
        );
        let spawned =
            match spawn(agent, launch, |name| self.strips_env(name), &self.processes).await {
                Ok(spawned) => spawned,
                Err(e) => {
                    return TurnOutcome::Failed(spawn_failure(agent, label, &e, agent.dir.is_dir()))
                }
            };
        info!(
            "agent '{}': {label} turn running (pid {})",
            agent.name, spawned.process.pid
        );
        let _registered = Registered {
            processes: Arc::clone(&self.processes),
            agent: agent.name.clone(),
            process: Arc::clone(&spawned.process),
            graces: self.graces,
        };
        let read = read_turn(
            &self.harness,
            &agent.name,
            spawned.stdout,
            spawned.stderr,
            spawned.exit,
            StreamState::new(session.session_id.clone()),
            &updates,
        )
        .await;
        info!(
            "agent '{}': {label} exited with {} ({} text part(s))",
            agent.name,
            read.exit,
            read.state.texts.len()
        );
        outcome(agent, &self.harness, &read.state, &read.stderr, &read.exit)
    }

    /// The turn's process ended with the old daemon: it never replied.
    async fn rewatch(
        &self,
        agent: &Agent,
        _session: SessionRef,
        _request: TurnRequest,
        _io: TurnIo,
    ) -> TurnOutcome {
        TurnOutcome::Failed(cut_off(agent, self.harness.label()))
    }

    /// Stop the turn's process and wait until it is gone (also one a
    /// dropped turn is stopping already).
    async fn stop(&self, agent: &Agent, _session: &SessionRef) -> Result<(), String> {
        let Some(process) = self.processes.get(&agent.name) else {
            return Ok(());
        };
        let stopped = process.stop(&self.graces).await;
        self.processes.remove(&agent.name, &process);
        stopped.map_err(|e| format!("{}: {e}", self.harness.label()))
    }

    /// Nothing is remembered between turns.
    fn forget(&self, _session: &SessionRef) {}

    /// No hooks.
    fn hook_matches(&self, _payload: &Value, _session: &SessionRef) -> bool {
        false
    }

    /// Between turns no process runs: idle.
    async fn session_phase(
        &self,
        _agent: &Agent,
        _session: &SessionRef,
    ) -> Result<AgentPhase, String> {
        Ok(AgentPhase::Idle)
    }

    async fn session_phases(
        &self,
        sessions: &[(Arc<Agent>, SessionRef)],
    ) -> Vec<Result<AgentPhase, String>> {
        vec![Ok(AgentPhase::Idle); sessions.len()]
    }

    /// No hooks.
    async fn idle_event(&self, _payload: &Value, _session: &SessionRef) -> IdleEvent {
        IdleEvent::Ignored
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::progress::Feedback;
    use crate::agent::{Harness, Permissions};
    use std::path::{Path, PathBuf};
    use tokio::sync::mpsc;

    /// A harness run by `sh -c <script>`, with a line protocol of its own:
    /// `S <id>` a session, `T <message> <text>` a text, `U <tool>` a tool
    /// call, `E <message>` an error; any other line is a text of message
    /// `raw`. A stderr line `gone` is a session it no longer has.
    struct Fake {
        script: String,
    }

    impl TurnHarness for Fake {
        fn label(&self) -> &'static str {
            "Fake"
        }

        fn launch(
            &self,
            _agent: &Agent,
            _session: &SessionRef,
            request: &TurnRequest,
        ) -> Result<Launch, String> {
            Ok(Launch {
                args: vec!["-c".into(), self.script.clone(), "fake".into()],
                stdin: Some(request.text.clone()),
                env: vec![("FAKE_SET".into(), "set by launch".into())],
            })
        }

        fn strips_env(&self, name: &str) -> bool {
            name == "FAKE_PARENT"
        }

        fn events(&self, line: &str) -> Vec<StreamEvent> {
            let line = line.trim_end();
            let (kind, rest) = line.split_once(' ').unwrap_or((line, ""));
            vec![match kind {
                "S" => StreamEvent::Session(rest.to_string()),
                "T" => {
                    let (message, text) = rest.split_once(' ').unwrap_or((rest, ""));
                    StreamEvent::Text {
                        message: message.to_string(),
                        text: text.to_string(),
                    }
                }
                "U" => StreamEvent::Tool(rest.to_string()),
                "E" => StreamEvent::Error(rest.to_string()),
                _ => StreamEvent::Text {
                    message: "raw".to_string(),
                    text: line.to_string(),
                },
            }]
        }

        fn stderr_failure(&self, agent: &Agent, stderr: &[String]) -> Option<String> {
            stderr
                .iter()
                .any(|l| l == "gone")
                .then(|| format!("Agent '{}': its session is gone.", agent.name))
        }
    }

    fn harness() -> Fake {
        Fake {
            script: String::new(),
        }
    }

    fn agent(dir: &Path) -> Agent {
        Agent {
            name: "coder".into(),
            hotkey: None,
            new_session_hotkey: None,
            harness: Harness::OpenCode,
            dir: dir.to_path_buf(),
            permissions: Permissions::Auto,
            command: "/bin/sh".into(),
            args: Vec::new(),
            instruction: "Speak.".into(),
            feedback: Feedback::Brief,
        }
    }

    fn request(text: &str) -> TurnRequest {
        TurnRequest {
            text: text.to_string(),
            started_at: chrono::Utc::now(),
        }
    }

    fn turn_io() -> (TurnIo, mpsc::UnboundedReceiver<TurnUpdate>) {
        let (updates, rx) = TurnUpdates::channel();
        let (_hooks_tx, hooks) = mpsc::unbounded_channel();
        (TurnIo { hooks, updates }, rx)
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<TurnUpdate>) -> Vec<TurnUpdate> {
        let mut out = Vec::new();
        while let Ok(update) = rx.try_recv() {
            out.push(update);
        }
        out
    }

    fn session(id: &str) -> TurnUpdate {
        TurnUpdate::Session(SessionRef {
            session_id: Some(id.to_string()),
            bg_id: None,
            transcript_path: None,
        })
    }

    fn text(message: &str, text: &str) -> StreamEvent {
        StreamEvent::Text {
            message: message.into(),
            text: text.into(),
        }
    }

    #[test]
    fn a_session_id_is_reported_only_when_it_is_new() {
        let (updates, mut rx) = TurnUpdates::channel();
        // A resumed turn's own id: nothing to report.
        let mut state = StreamState::new(Some("ses_a".into()));
        state.event(StreamEvent::Session("ses_a".into()), &updates);
        assert!(drain(&mut rx).is_empty());
        // Another one (a harness that started a new session): reported once.
        state.event(StreamEvent::Session("ses_b".into()), &updates);
        state.event(StreamEvent::Session("ses_b".into()), &updates);
        assert_eq!(drain(&mut rx), [session("ses_b")]);
        // A new session's first line names it.
        let mut state = StreamState::new(None);
        state.event(StreamEvent::Session("ses_c".into()), &updates);
        assert_eq!(drain(&mut rx), [session("ses_c")]);
        assert_eq!(
            updates.latest().unwrap().session_id.as_deref(),
            Some("ses_c")
        );
    }

    #[test]
    fn tools_are_activities_and_release_the_text_before_them() {
        let (updates, mut rx) = TurnUpdates::channel();
        let mut state = StreamState::new(None);
        state.event(text("m1", "<spoken>On it.</spoken> Checking."), &updates);
        state.event(StreamEvent::Tool("shell".into()), &updates);
        // The same activity again is not sent twice; nothing held.
        state.event(StreamEvent::Tool("bash".into()), &updates);
        state.event(StreamEvent::Tool("read".into()), &updates);
        state.event(text("m2", "Done."), &updates);
        state.event(text("m2", "<spoken>All done.</spoken>"), &updates);
        state.end();
        assert_eq!(
            drain(&mut rx),
            [
                TurnUpdate::Activity("Running a command".into()),
                TurnUpdate::Interim("<spoken>On it.</spoken> Checking.".into()),
                TurnUpdate::Activity("Reading files".into()),
            ]
        );
        // The reply is the last message's parts, joined.
        assert_eq!(
            state.reply().as_deref(),
            Some("Done.\n<spoken>All done.</spoken>")
        );
    }

    #[test]
    fn text_text_end_speaks_nothing_extra() {
        let (updates, mut rx) = TurnUpdates::channel();
        let mut state = StreamState::new(None);
        state.event(text("m1", "<spoken>Found it.</spoken>"), &updates);
        state.event(text("m2", "<spoken>Fixed it.</spoken>"), &updates);
        state.end();
        assert!(drain(&mut rx).is_empty());
        assert_eq!(state.reply().as_deref(), Some("<spoken>Fixed it.</spoken>"));
        // No text, no reply.
        assert_eq!(StreamState::new(None).reply(), None);
        let mut blank = StreamState::new(None);
        blank.event(text("m1", "  "), &updates);
        assert_eq!(blank.reply(), None);
    }

    fn status(code: i32) -> Exit {
        use std::os::unix::process::ExitStatusExt;
        Exit::Status(ExitStatus::from_raw(code << 8))
    }

    #[test]
    fn the_outcome_prefers_an_error_then_the_reply_then_stderr() {
        let a = agent(Path::new("/proj"));
        let h = harness();
        let (updates, _rx) = TurnUpdates::channel();
        let mut replied = StreamState::new(None);
        replied.event(text("m", "Done. <spoken>Done.</spoken>"), &updates);
        assert_eq!(
            outcome(&a, &h, &replied, &[], &status(0)),
            TurnOutcome::Reply("Done. <spoken>Done.</spoken>".into())
        );
        // A reply, then a non-zero exit: still the reply.
        assert_eq!(
            outcome(&a, &h, &replied, &[], &status(1)),
            TurnOutcome::Reply("Done. <spoken>Done.</spoken>".into())
        );
        // An error the harness reported wins, on one line.
        let mut failed = StreamState::new(None);
        failed.event(text("m", "Partial."), &updates);
        failed.event(
            StreamEvent::Error("Could not connect.\n  Check the port".into()),
            &updates,
        );
        assert_eq!(
            outcome(&a, &h, &failed, &[], &status(1)),
            TurnOutcome::Failed(
                "Agent 'coder': Fake failed: Could not connect. Check the port.".into()
            )
        );
        let empty = StreamState::new(None);
        assert_eq!(
            outcome(&a, &h, &empty, &[], &status(0)),
            TurnOutcome::Failed("Agent 'coder': Fake ended the turn without a reply.".into())
        );
        let stderr = vec![
            "\u{1b}[93m\u{1b}[1m! \u{1b}[0mpermission requested: shell; auto-rejecting".into(),
            "error: unknown option --standalone".into(),
            "see --help".into(),
        ];
        assert_eq!(
            outcome(&a, &h, &empty, &stderr, &status(2)),
            TurnOutcome::Failed(
                "Agent 'coder': Fake failed: error: unknown option --standalone.".into()
            )
        );
        assert_eq!(
            outcome(&a, &h, &empty, &[], &status(3)),
            TurnOutcome::Failed(
                "Agent 'coder': Fake failed: it exited with exit status: 3.".into()
            )
        );
        // What the harness recognizes in stderr comes before the generic line,
        // but never before an error event or a reply.
        let gone = vec!["error: thread not found".into(), "gone".into()];
        assert_eq!(
            outcome(&a, &h, &empty, &gone, &status(1)),
            TurnOutcome::Failed("Agent 'coder': its session is gone.".into())
        );
        assert_eq!(
            outcome(&a, &h, &replied, &gone, &status(1)),
            TurnOutcome::Reply("Done. <spoken>Done.</spoken>".into())
        );
        assert_eq!(
            outcome(&a, &h, &empty, &gone, &status(0)),
            TurnOutcome::Failed("Agent 'coder': Fake ended the turn without a reply.".into())
        );
    }

    #[test]
    fn stderr_details_skip_colors_and_prefer_an_error_line() {
        assert_eq!(
            stderr_detail(&[
                "\u{1b}[1mwarning: slow\u{1b}[0m".into(),
                " last line ".into()
            ]),
            Some("last line".into())
        );
        assert_eq!(stderr_detail(&["".into(), "  ".into()]), None);
        assert_eq!(
            strip_ansi("\u{1b}[93m\u{1b}[1m!\u{1b}[0m permission\u{1b}7"),
            "! permission"
        );
    }

    #[test]
    fn every_driver_strips_a_parent_sessions_variables_and_whisrs_keys() {
        let driver = ProcessDriver::new(harness());
        for name in [
            "CLAUDECODE",
            "CLAUDE_CODE_SESSION_ID",
            "WHISRS_GROQ_API_KEY",
            "FAKE_PARENT",
        ] {
            assert!(driver.strips_env(name), "{name}");
        }
        for name in ["PATH", "HOME", "CLAUDE_CONFIG_DIR", "FAKE_SET"] {
            assert!(!driver.strips_env(name), "{name}");
        }
    }

    #[test]
    fn group_targets_never_name_the_daemon_or_everyone() {
        assert_eq!(group_target(0), None);
        assert_eq!(group_target(1), None);
        assert_eq!(group_target(u32::MAX), None);
        assert_eq!(group_target(4242), Some(-4242));
    }

    #[test]
    fn random_bytes_differ() {
        assert_ne!(random_bytes::<16>(), random_bytes::<16>());
    }

    fn fake(script: &str) -> Arc<ProcessDriver<Fake>> {
        Arc::new(ProcessDriver::with_graces(
            Fake {
                script: script.to_string(),
            },
            [
                Duration::from_millis(600),
                Duration::from_millis(600),
                Duration::from_millis(1500),
            ],
        ))
    }

    /// A whole turn: the prompt on stdin (closed, so `cat` ends), the
    /// session reported, the activity and the interim, then the reply.
    #[tokio::test]
    async fn a_turn_reads_the_process_to_its_reply() {
        let dir = tempfile::tempdir().unwrap();
        let driver = fake(
            "read -r prompt; echo \"S ses_new\"; echo \"T m1 <spoken>On it.</spoken>\"; \
             echo \"U shell\"; echo \"T m2 You said: $prompt <spoken>Done.</spoken>\"",
        );
        let (io, mut rx) = turn_io();
        let outcome = driver
            .run_turn(
                &agent(dir.path()),
                SessionRef::default(),
                request("hello there"),
                io,
            )
            .await;
        assert_eq!(
            outcome,
            TurnOutcome::Reply("You said: hello there <spoken>Done.</spoken>".into())
        );
        assert_eq!(
            drain(&mut rx),
            [
                session("ses_new"),
                TurnUpdate::Activity("Running a command".into()),
                TurnUpdate::Interim("<spoken>On it.</spoken>".into()),
            ]
        );
        // Nothing is left registered once the process exited.
        assert!(driver.processes.get("coder").is_none());
    }

    #[tokio::test]
    async fn the_child_runs_in_the_dir_with_pwd_and_without_stripped_variables() {
        let dir = tempfile::tempdir().unwrap();
        let driver =
            fake("echo \"pwd=$(pwd) PWD=$PWD set=$FAKE_SET home=${HOME:-none}\"; cat >/dev/null");
        let mut a = agent(dir.path());
        a.name = "env".into();
        let (io, _rx) = turn_io();
        // `HOME` stands in for a stripped variable: the test process has it.
        let spawned = spawn(
            &a,
            driver
                .harness
                .launch(&a, &SessionRef::default(), &request("x"))
                .unwrap(),
            |name| name == "HOME",
            &driver.processes,
        )
        .await
        .unwrap();
        let read = read_turn(
            &driver.harness,
            "env",
            spawned.stdout,
            spawned.stderr,
            spawned.exit,
            StreamState::new(None),
            &io.updates,
        )
        .await;
        let dir = dir.path();
        assert_eq!(
            read.state.reply().as_deref(),
            Some(format!("pwd={0} PWD={0} set=set by launch home=none", dir.display()).as_str())
        );
        assert!(read.exit.success(), "{}", read.exit);
    }

    #[tokio::test]
    async fn a_missing_command_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let driver = fake("true");
        let mut a = agent(dir.path());
        a.command = "/nonexistent/fake-harness".into();
        let (io, _rx) = turn_io();
        let TurnOutcome::Failed(message) = driver
            .run_turn(&a, SessionRef::default(), request("x"), io)
            .await
        else {
            panic!("a missing command must fail the turn");
        };
        assert!(
            message.contains("`/nonexistent/fake-harness` was not found. Install Fake"),
            "{message}"
        );
        let mut gone = agent(&PathBuf::from("/nonexistent/dir"));
        gone.command = "/bin/sh".into();
        let (io, _rx) = turn_io();
        let TurnOutcome::Failed(message) = driver
            .run_turn(&gone, SessionRef::default(), request("x"), io)
            .await
        else {
            panic!("a missing dir must fail the turn");
        };
        assert!(message.contains("no longer exists"), "{message}");
    }

    /// Wait until `path` holds a pid: the script got that far.
    async fn wait_for_pid(path: &Path) -> u32 {
        for _ in 0..200 {
            if let Some(pid) = std::fs::read_to_string(path)
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the script never wrote {}", path.display());
    }

    fn alive(pid: u32) -> bool {
        // A zombie is gone for this purpose: reaped by whoever owns it.
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| claude::stat_state(&stat))
            .is_some_and(|state| state != 'Z')
    }

    /// Start a turn running `script` (it writes its background child's pid
    /// to `pid`), wait until it runs, then stop it as `--stop` does: abort
    /// the task, then `Driver::stop`. How long the stop took.
    async fn stop_after_start(script: &str, dir: &Path) -> (Duration, u32, u32) {
        let driver = fake(script);
        let a = Arc::new(agent(dir));
        let (io, _rx) = turn_io();
        let task = {
            let (driver, a) = (Arc::clone(&driver), Arc::clone(&a));
            tokio::spawn(async move {
                driver
                    .run_turn(&a, SessionRef::default(), request("x"), io)
                    .await
            })
        };
        let child = wait_for_pid(&dir.join("pid")).await;
        let leader = driver.processes.get("coder").expect("registered").pid;
        let started = Instant::now();
        task.abort();
        driver
            .stop(&a, &SessionRef::default())
            .await
            .expect("the stop ends the group");
        let took = started.elapsed();
        assert!(driver.processes.get("coder").is_none());
        (took, leader, child)
    }

    /// SIGINT ignored (by the script, and by its background job as by
    /// every shell's): SIGTERM after the first grace ends both.
    #[tokio::test]
    async fn a_stop_escalates_past_an_ignored_sigint() {
        let dir = tempfile::tempdir().unwrap();
        let script = "trap '' INT; sleep 30 & echo $! > pid; wait";
        let (took, leader, child) = stop_after_start(script, dir.path()).await;
        // SIGINT did nothing for a grace, SIGTERM ended both.
        assert!(took >= Duration::from_millis(600), "{took:?}");
        assert!(took < Duration::from_millis(1200 + 600), "{took:?}");
        assert!(!alive(leader) && !alive(child));
        assert!(!group_alive(leader));
    }

    #[tokio::test]
    async fn a_stop_kills_what_ignores_sigint_and_sigterm() {
        let dir = tempfile::tempdir().unwrap();
        let script = "trap '' INT TERM; sleep 30 & echo $! > pid; wait";
        let (took, leader, child) = stop_after_start(script, dir.path()).await;
        assert!(took >= Duration::from_millis(1200), "{took:?}");
        assert!(took < Duration::from_millis(1200 + 1500), "{took:?}");
        assert!(!alive(leader) && !alive(child));
    }

    /// SIGINT ends a harness that does not ignore it, with no escalation.
    /// (A shell's background job ignores SIGINT, so the script execs.)
    #[tokio::test]
    async fn sigint_alone_ends_a_harness_that_handles_it() {
        let dir = tempfile::tempdir().unwrap();
        let script = "echo $$ > pid; exec sleep 30";
        let (took, leader, child) = stop_after_start(script, dir.path()).await;
        // Before the SIGTERM a first grace later.
        assert!(took < Duration::from_millis(600), "{took:?}");
        assert!(!alive(leader) && !alive(child));
    }

    /// A turn dropped with no stop (its task aborted, as when a new turn
    /// replaces it) still has its process stopped.
    #[tokio::test]
    async fn a_dropped_turn_stops_its_process() {
        let dir = tempfile::tempdir().unwrap();
        let driver = fake("trap '' INT; sleep 30 & echo $! > pid; wait");
        let a = Arc::new(agent(dir.path()));
        let (io, _rx) = turn_io();
        let task = {
            let (driver, a) = (Arc::clone(&driver), Arc::clone(&a));
            tokio::spawn(async move {
                driver
                    .run_turn(&a, SessionRef::default(), request("x"), io)
                    .await
            })
        };
        let child = wait_for_pid(&dir.path().join("pid")).await;
        task.abort();
        for _ in 0..100 {
            if driver.processes.get("coder").is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(driver.processes.get("coder").is_none());
        assert!(!alive(child));
    }

    #[tokio::test]
    async fn a_stop_with_no_turn_running_does_nothing() {
        let driver = fake("true");
        let a = agent(Path::new("/proj"));
        assert_eq!(driver.stop(&a, &SessionRef::default()).await, Ok(()));
    }

    #[tokio::test]
    async fn between_turns_nothing_runs_and_a_rewatch_ends_at_once() {
        let driver = fake("true");
        let a = Arc::new(agent(Path::new("/proj")));
        let s = SessionRef {
            session_id: Some("ses_x".into()),
            bg_id: None,
            transcript_path: None,
        };
        assert_eq!(driver.session_phase(&a, &s).await, Ok(AgentPhase::Idle));
        assert_eq!(
            driver.session_phases(&[(Arc::clone(&a), s.clone())]).await,
            vec![Ok(AgentPhase::Idle)]
        );
        let payload = serde_json::json!({"session_id": "ses_x"});
        assert!(!driver.hook_matches(&payload, &s));
        assert_eq!(driver.idle_event(&payload, &s).await, IdleEvent::Ignored);
        let (io, _rx) = turn_io();
        let TurnOutcome::Failed(message) = driver.rewatch(&a, s, request(""), io).await else {
            panic!("a rewatch ends the turn");
        };
        assert_eq!(message, cut_off(&a, "Fake"));
        assert!(
            message.contains("whisrs restarted during that turn"),
            "{message}"
        );
        assert!(!message.contains('\u{2014}'));
    }
}
