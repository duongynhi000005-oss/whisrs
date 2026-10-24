//! Agent mode runtime (`[[agents]]`): per-agent slots, running turns, the
//! reply queue, the IPC handlers and the hook events.
//!
//! Lives in [`DaemonContext`] behind its own lock, outside [`DaemonState`]:
//! the dictation state machine only gains the agent recording sink (a voice
//! turn records exactly like command mode's instruction, then hands the
//! transcript here). A turn runs on its own task, so the state machine is
//! back to Idle as soon as the transcript is ready and dictation keeps
//! working while agents run. Replies wait in a FIFO and are spoken when the
//! daemon is Idle.
//!
//! The runtime is harness-neutral: it calls [`Driver`] and never names a
//! harness outside [`AgentRuntime::new`].
//!
//! With no usable `[[agents]]` entry nothing here runs: no task, no process,
//! no file read or written.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use tokio::sync::{mpsc, Mutex, Notify};
use tracing::{debug, error, info, warn};

use whisrs::agent::claude::ClaudeDriver;
use whisrs::agent::store::{self, AgentRecord, AgentStore};
use whisrs::agent::{
    resolve_agents, spoken, Agent, AgentAction, AgentInfo, AgentPhase, Driver, Harness, SessionRef,
    TurnIo, TurnOutcome, TurnRequest, TurnUpdate,
};
use whisrs::audio::capture::AudioCaptureHandle;
use whisrs::audio::feedback;
use whisrs::state::Action;
use whisrs::{Config, Response, State};

use crate::command_mode::{claim_session, collect_instruction_audio};
use crate::context::{AgentRecordingContext, DaemonContext, DaemonState};
use crate::notify::{send_notification, send_notification_for};
use crate::pipeline::{
    format_api_error, format_no_microphone_error, transcribe_batch_audio, BatchOptions,
};
#[cfg(feature = "recording")]
use crate::recording::{refuse_while_recording, DictationStart};
use crate::speak::{speak_text, stop_read_aloud, tts_backend, SpeakAttempt, SpeakOrigin};

/// The refusal for a press while that agent's turn runs. Spoken and
/// notified where it happens, so the hotkey dispatcher does not notify it a
/// second time ([`error_already_notified`]).
const STILL_WORKING: &str = "Still working on the last one.";

/// How long a hook event no turn claimed is kept: a first turn's `Stop` can
/// arrive before `claude agents` has told us its session id.
const UNCLAIMED_TTL: Duration = Duration::from_secs(120);

/// At most this many unclaimed hook events are kept.
const UNCLAIMED_MAX: usize = 32;

/// How long a reply notification stays up.
const REPLY_NOTIFICATION_MS: u32 = 10_000;

/// Whether an agent error was already notified where it happened: the
/// still-working refusal, and the refusal while a recording runs
/// (`refuse_while_recording` notifies it).
pub(crate) fn error_already_notified(message: &str) -> bool {
    message.starts_with(STILL_WORKING) || message.starts_with("A recording is in progress.")
}

/// One running turn.
struct RunningTurn {
    /// Tells a finished task from the one that replaced it.
    generation: u64,
    /// When it started: only hook events that arrived later can be its own.
    started: Instant,
    task: tokio::task::JoinHandle<()>,
    /// Hook payloads for the driver.
    hooks: mpsc::UnboundedSender<serde_json::Value>,
}

/// What the runtime holds per agent at run time (the persisted part is the
/// agent's [`AgentRecord`]).
struct Slot {
    phase: AgentPhase,
    turn: Option<RunningTurn>,
}

impl Default for Slot {
    fn default() -> Self {
        Self {
            phase: AgentPhase::Idle,
            turn: None,
        }
    }
}

struct Inner {
    store: AgentStore,
    slots: HashMap<String, Slot>,
    /// Hook events no running turn matched yet, with when they arrived.
    unclaimed: VecDeque<(Instant, serde_json::Value)>,
    next_generation: u64,
}

/// One queued utterance.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Utterance {
    agent: String,
    text: String,
}

/// The agent runtime.
pub(crate) struct AgentRuntime {
    agents: Vec<Arc<Agent>>,
    drivers: HashMap<Harness, Arc<dyn Driver>>,
    store_path: PathBuf,
    inner: Mutex<Inner>,
    /// Replies and notices waiting for the daemon to be Idle.
    utterances: std::sync::Mutex<VecDeque<Utterance>>,
    utterance_ready: Notify,
    /// TTS is enabled: utterances are queued at all.
    speak: bool,
}

impl AgentRuntime {
    /// The runtime for `config`'s usable agents. With none it is inert and
    /// reads no file. The warnings for skipped entries are logged by
    /// `Config::validate`.
    pub(crate) fn new(config: &Config, socket: &Path) -> Self {
        let (agents, _warnings) = resolve_agents(&config.agents);
        if agents.is_empty() {
            return Self::disabled();
        }
        let store_path = store::store_path();
        let (mut store, load_error) = store::load(&store_path);
        if let Some(e) = load_error {
            warn!("agents: {e}");
        }
        // A changed harness or dir starts a fresh session.
        let mut changed = false;
        for agent in &agents {
            if let Some(record) = store.agents.get(&agent.name) {
                if !record.matches(agent.harness.as_str(), &agent.dir) {
                    info!(
                        "agent '{}': harness or dir changed, the next turn starts a new session",
                        agent.name
                    );
                    store.agents.insert(
                        agent.name.clone(),
                        AgentRecord::new(agent.harness.as_str(), &agent.dir),
                    );
                    changed = true;
                }
            }
        }
        if changed {
            if let Err(e) = store::save(&store_path, &store) {
                warn!("agents: cannot write {}: {e:#}", store_path.display());
            }
        }
        let whisrs = whisrs_binary();
        info!(
            "agents: {} configured ({}); hooks run {}",
            agents.len(),
            agents
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            whisrs.display()
        );
        let mut drivers: HashMap<Harness, Arc<dyn Driver>> = HashMap::new();
        drivers.insert(
            Harness::Claude,
            Arc::new(ClaudeDriver::new(&whisrs, socket)),
        );
        Self {
            agents: agents.into_iter().map(Arc::new).collect(),
            drivers,
            store_path,
            inner: Mutex::new(Inner {
                store,
                slots: HashMap::new(),
                unclaimed: VecDeque::new(),
                next_generation: 1,
            }),
            utterances: std::sync::Mutex::new(VecDeque::new()),
            utterance_ready: Notify::new(),
            speak: config.tts.as_ref().is_some_and(|t| t.enabled),
        }
    }

    /// A runtime with no agents.
    pub(crate) fn disabled() -> Self {
        Self {
            agents: Vec::new(),
            drivers: HashMap::new(),
            store_path: PathBuf::new(),
            inner: Mutex::new(Inner {
                store: AgentStore::default(),
                slots: HashMap::new(),
                unclaimed: VecDeque::new(),
                next_generation: 1,
            }),
            utterances: std::sync::Mutex::new(VecDeque::new()),
            utterance_ready: Notify::new(),
            speak: false,
        }
    }

    /// Whether any agent is configured.
    pub(crate) fn is_enabled(&self) -> bool {
        !self.agents.is_empty()
    }

    /// The usable agents (for the hotkey listener).
    pub(crate) fn agents(&self) -> Vec<Agent> {
        self.agents.iter().map(|a| Agent::clone(a)).collect()
    }

    /// Whether any agent has a hotkey.
    pub(crate) fn has_hotkeys(&self) -> bool {
        self.agents.iter().any(|a| a.hotkey.is_some())
    }

    fn agent(&self, name: &str) -> Option<Arc<Agent>> {
        self.agents.iter().find(|a| a.name == name).cloned()
    }

    fn driver(&self, harness: Harness) -> Arc<dyn Driver> {
        Arc::clone(
            self.drivers
                .get(&harness)
                .expect("every resolvable harness has a driver"),
        )
    }

    fn save(&self, inner: &Inner) {
        if let Err(e) = store::save(&self.store_path, &inner.store) {
            warn!("agents: cannot write {}: {e:#}", self.store_path.display());
        }
    }

    /// Queue `text` to be spoken when the daemon is Idle (TTS on only).
    fn say(&self, agent: &str, text: String) {
        if !self.speak {
            return;
        }
        if let Ok(mut queue) = self.utterances.lock() {
            queue.push_back(Utterance {
                agent: agent.to_string(),
                text,
            });
        }
        self.utterance_ready.notify_one();
    }

    fn next_utterance(&self) -> Option<Utterance> {
        self.utterances.lock().ok()?.pop_front()
    }

    fn requeue_front(&self, utterance: Utterance) {
        if let Ok(mut queue) = self.utterances.lock() {
            queue.push_front(utterance);
        }
    }

    fn has_utterances(&self) -> bool {
        self.utterances.lock().is_ok_and(|q| !q.is_empty())
    }

    /// The row `whisrs agent` shows for `agent`.
    fn info(inner: &Inner, agent: &Agent) -> AgentInfo {
        let phase = inner
            .slots
            .get(&agent.name)
            .map(|s| s.phase)
            .unwrap_or(AgentPhase::Idle);
        let session = inner
            .store
            .agents
            .get(&agent.name)
            .and_then(|r| r.session_id.as_deref())
            .map(|id| id.chars().take(8).collect());
        AgentInfo {
            name: agent.name.clone(),
            harness: agent.harness.to_string(),
            phase,
            session,
        }
    }

    async fn list(&self) -> Vec<AgentInfo> {
        let inner = self.inner.lock().await;
        self.agents.iter().map(|a| Self::info(&inner, a)).collect()
    }

    async fn phase(&self, name: &str) -> AgentPhase {
        let inner = self.inner.lock().await;
        inner
            .slots
            .get(name)
            .map(|s| s.phase)
            .unwrap_or(AgentPhase::Idle)
    }
}

/// The `whisrs` binary the hooks run: the one beside this `whisrsd`, else
/// `whisrs` on the session's `PATH`.
fn whisrs_binary() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("whisrs")))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("whisrs"))
}

/// The record of `agent`, created (or reset, after a harness or dir change)
/// as needed.
fn record_mut<'a>(inner: &'a mut Inner, agent: &Agent) -> &'a mut AgentRecord {
    let fresh = || AgentRecord::new(agent.harness.as_str(), &agent.dir);
    let record = inner
        .store
        .agents
        .entry(agent.name.clone())
        .or_insert_with(fresh);
    if !record.matches(agent.harness.as_str(), &agent.dir) {
        *record = fresh();
    }
    record
}

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

/// Start the reply queue and watch again the turns a restart interrupted.
/// Called once, only with agents configured.
pub(crate) async fn start_background(
    context: Arc<DaemonContext>,
    daemon_state: Arc<Mutex<DaemonState>>,
    state_rx: tokio::sync::watch::Receiver<State>,
) {
    tokio::spawn(utterance_loop(
        Arc::clone(&context),
        Arc::clone(&daemon_state),
        state_rx,
    ));

    let rt = &context.agents;
    let mut inner = rt.inner.lock().await;
    for agent in &rt.agents {
        let Some(record) = inner.store.agents.get(&agent.name) else {
            continue;
        };
        if !record.turn_running {
            continue;
        }
        let session = record.session();
        let started_at = record.turn_started_at.unwrap_or(record.updated_at);
        info!(
            "agent '{}': a turn was running before the restart; watching session {} again",
            agent.name,
            session.session_id.as_deref().unwrap_or("?")
        );
        let request = TurnRequest {
            text: String::new(),
            started_at,
        };
        spawn_turn(
            &context,
            &mut inner,
            Arc::clone(agent),
            session,
            request,
            true,
        );
    }
}

/// Speak queued replies and notices one at a time, each once the daemon is
/// Idle. Watches the state channel like `hook_dispatch_loop`.
async fn utterance_loop(
    context: Arc<DaemonContext>,
    daemon_state: Arc<Mutex<DaemonState>>,
    mut state_rx: tokio::sync::watch::Receiver<State>,
) {
    loop {
        if !context.agents.has_utterances() {
            context.agents.utterance_ready.notified().await;
            continue;
        }
        if *state_rx.borrow_and_update() != State::Idle {
            if state_rx.changed().await.is_err() {
                return;
            }
            continue;
        }
        let Some(utterance) = context.agents.next_utterance() else {
            continue;
        };
        let backend = match tts_backend(&context) {
            Ok(b) => b,
            Err(e) => {
                warn!("agent '{}': cannot speak the reply: {e}", utterance.agent);
                continue;
            }
        };
        debug!(
            "agent '{}': speaking {} chars",
            utterance.agent,
            utterance.text.len()
        );
        match speak_text(
            Arc::clone(&daemon_state),
            Arc::clone(&context),
            backend,
            &utterance.text,
            SpeakOrigin::Agent,
        )
        .await
        {
            SpeakAttempt::NotIdle(_) => {
                // Something started first: wait for Idle again.
                context.agents.requeue_front(utterance);
                if state_rx.changed().await.is_err() {
                    return;
                }
            }
            SpeakAttempt::Ran(Response::Error { message }) => {
                warn!("agent '{}': reply not spoken: {message}", utterance.agent);
            }
            SpeakAttempt::Ran(_) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// IPC
// ---------------------------------------------------------------------------

/// `{"cmd":"agent"}`.
pub(crate) async fn handle_agent(
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
    action: AgentAction,
    name: Option<String>,
    text: Option<String>,
    new: bool,
) -> Response {
    let rt = &context.agents;
    if action == AgentAction::List {
        return Response::Agents {
            agents: rt.list().await,
        };
    }
    if !rt.is_enabled() {
        return error(
            "no agents are configured: add an [[agents]] entry to config.toml \
             (see docs/configuration.md)",
        );
    }
    let Some(name) = name.filter(|n| !n.trim().is_empty()) else {
        return error("which agent? Pass its name: whisrs agent <name>");
    };
    let Some(agent) = rt.agent(&name) else {
        let known: Vec<&str> = rt.agents.iter().map(|a| a.name.as_str()).collect();
        return error(&format!(
            "no agent named '{name}' (configured: {}). An entry the config warned about is \
             skipped; see the daemon log.",
            known.join(", ")
        ));
    };
    match action {
        AgentAction::List => unreachable!("answered above"),
        AgentAction::Toggle => voice_toggle(daemon_state, context, agent).await,
        AgentAction::Send => {
            let Some(text) = text.filter(|t| !t.trim().is_empty()) else {
                return error("nothing to send: the text is empty");
            };
            if new {
                if let Err(message) = reset(&context, &agent).await {
                    return error(&message);
                }
            }
            match start_turn(&context, &agent, text.trim().to_string()).await {
                Ok(()) => agent_row(&context, &agent).await,
                Err(message) => {
                    if message == STILL_WORKING {
                        refuse_still_working(&context, &agent);
                    }
                    error(&message)
                }
            }
        }
        AgentAction::Reset => match reset(&context, &agent).await {
            Ok(()) => agent_row(&context, &agent).await,
            Err(message) => error(&message),
        },
        AgentAction::Stop => match stop_turn(&context, &agent).await {
            Ok(()) => agent_row(&context, &agent).await,
            Err(message) => error(&message),
        },
    }
}

fn error(message: &str) -> Response {
    Response::Error {
        message: message.to_string(),
    }
}

async fn agent_row(context: &DaemonContext, agent: &Agent) -> Response {
    let inner = context.agents.inner.lock().await;
    Response::Agents {
        agents: vec![AgentRuntime::info(&inner, agent)],
    }
}

/// `{"cmd":"agent-event"}`: route a hook payload to the running turn whose
/// stored session it belongs to, or keep it briefly for a turn that does not
/// know its session id yet.
pub(crate) async fn handle_agent_event(context: &DaemonContext, payload: serde_json::Value) {
    let rt = &context.agents;
    if !rt.is_enabled() {
        debug!("agent-event ignored: no agents configured");
        return;
    }
    let mut inner = rt.inner.lock().await;
    let mut target = None;
    for agent in &rt.agents {
        let Some(turn) = inner.slots.get(&agent.name).and_then(|s| s.turn.as_ref()) else {
            continue;
        };
        let Some(record) = inner.store.agents.get(&agent.name) else {
            continue;
        };
        if rt
            .driver(agent.harness)
            .hook_matches(&payload, &record.session())
        {
            target = Some((agent.name.clone(), turn.hooks.clone()));
            break;
        }
    }
    match target {
        Some((name, hooks)) => {
            debug!("agent '{name}': hook event routed");
            let _ = hooks.send(payload);
        }
        None => {
            let now = Instant::now();
            inner
                .unclaimed
                .retain(|(at, _)| now.duration_since(*at) < UNCLAIMED_TTL);
            if inner.unclaimed.len() >= UNCLAIMED_MAX {
                inner.unclaimed.pop_front();
            }
            debug!("agent-event: no running turn matches yet; kept for a moment");
            inner.unclaimed.push_back((now, payload));
        }
    }
}

// ---------------------------------------------------------------------------
// Turns
// ---------------------------------------------------------------------------

/// Start a turn for `agent`. Refused while its turn is working; a turn that
/// waits for the user (needs-you) is replaced, so answering it by voice is
/// one press.
async fn start_turn(
    context: &Arc<DaemonContext>,
    agent: &Arc<Agent>,
    text: String,
) -> Result<(), String> {
    let rt = &context.agents;
    let mut inner = rt.inner.lock().await;
    let slot = inner.slots.entry(agent.name.clone()).or_default();
    if let Some(turn) = &slot.turn {
        if slot.phase != AgentPhase::NeedsYou {
            return Err(STILL_WORKING.to_string());
        }
        info!(
            "agent '{}': a new turn answers the one waiting for you",
            agent.name
        );
        turn.task.abort();
        slot.turn = None;
    }
    let now = Utc::now();
    let record = record_mut(&mut inner, agent);
    record.turn_running = true;
    record.turn_started_at = Some(now);
    record.updated_at = now;
    let session = record.session();
    rt.save(&inner);
    info!(
        "agent '{}': starting a turn ({} chars) in {}",
        agent.name,
        text.chars().count(),
        match &session.session_id {
            Some(id) => format!("session {id}"),
            None => "a new session".to_string(),
        }
    );
    let request = TurnRequest {
        text,
        started_at: now,
    };
    spawn_turn(
        context,
        &mut inner,
        Arc::clone(agent),
        session,
        request,
        false,
    );
    Ok(())
}

/// Spawn the task that runs (or, after a restart, watches) a turn and
/// register it in the agent's slot.
fn spawn_turn(
    context: &Arc<DaemonContext>,
    inner: &mut Inner,
    agent: Arc<Agent>,
    session: SessionRef,
    request: TurnRequest,
    rewatch: bool,
) {
    let generation = inner.next_generation;
    inner.next_generation += 1;
    let (hooks_tx, hooks_rx) = mpsc::unbounded_channel();
    let started = Instant::now();
    // Events this session sent before the turn started belong to an earlier
    // turn (or one the session ran on its own, like a follow-up after a
    // background command): drop them so they cannot end this one.
    forward_unclaimed(context, inner, &agent, &session, &hooks_tx, started);
    let ctx = Arc::clone(context);
    let task_agent = Arc::clone(&agent);
    let task = tokio::spawn(async move {
        run_turn_task(
            ctx, task_agent, generation, session, request, hooks_rx, rewatch,
        )
        .await;
    });
    let slot = inner.slots.entry(agent.name.clone()).or_default();
    slot.phase = AgentPhase::Working;
    slot.turn = Some(RunningTurn {
        generation,
        started,
        task,
        hooks: hooks_tx,
    });
}

/// Take every unclaimed hook event that belongs to `session`: the ones that
/// arrived at or after `since` (the turn's start) go to `hooks`, older ones
/// are dropped as stale.
fn forward_unclaimed(
    context: &DaemonContext,
    inner: &mut Inner,
    agent: &Agent,
    session: &SessionRef,
    hooks: &mpsc::UnboundedSender<serde_json::Value>,
    since: Instant,
) {
    if session.session_id.is_none() || inner.unclaimed.is_empty() {
        return;
    }
    let driver = context.agents.driver(agent.harness);
    let (fresh, stale) = take_session_events(
        &mut inner.unclaimed,
        |payload| driver.hook_matches(payload, session),
        since,
    );
    if stale > 0 {
        debug!(
            "agent '{}': dropped {stale} hook event(s) from before this turn",
            agent.name
        );
    }
    for payload in fresh {
        debug!("agent '{}': delivering an early hook event", agent.name);
        let _ = hooks.send(payload);
    }
}

/// Remove the events `belongs` accepts from `events`: those that arrived at
/// or after `since` are returned in order, older ones are counted and
/// dropped. Everything else stays.
fn take_session_events(
    events: &mut VecDeque<(Instant, serde_json::Value)>,
    belongs: impl Fn(&serde_json::Value) -> bool,
    since: Instant,
) -> (Vec<serde_json::Value>, usize) {
    let mut fresh = Vec::new();
    let mut stale = 0;
    let mut kept = VecDeque::new();
    for (at, payload) in events.drain(..) {
        if !belongs(&payload) {
            kept.push_back((at, payload));
        } else if at >= since {
            fresh.push(payload);
        } else {
            stale += 1;
        }
    }
    *events = kept;
    (fresh, stale)
}

async fn run_turn_task(
    context: Arc<DaemonContext>,
    agent: Arc<Agent>,
    generation: u64,
    session: SessionRef,
    request: TurnRequest,
    hooks: mpsc::UnboundedReceiver<serde_json::Value>,
    rewatch: bool,
) {
    let driver = context.agents.driver(agent.harness);
    let (updates_tx, mut updates_rx) = mpsc::unbounded_channel();
    let io = TurnIo {
        hooks,
        updates: updates_tx,
    };
    let turn = async {
        if rewatch {
            driver.rewatch(&agent, session, request, io).await
        } else {
            driver.run_turn(&agent, session, request, io).await
        }
    };
    tokio::pin!(turn);
    let outcome = loop {
        tokio::select! {
            outcome = &mut turn => break outcome,
            Some(update) = updates_rx.recv() => {
                apply_update(&context, &agent, generation, update).await;
            }
        }
    };
    while let Ok(update) = updates_rx.try_recv() {
        apply_update(&context, &agent, generation, update).await;
    }
    finish_turn(&context, &agent, generation, outcome).await;
}

/// Record a driver's progress for the turn `generation`.
async fn apply_update(context: &DaemonContext, agent: &Agent, generation: u64, update: TurnUpdate) {
    let rt = &context.agents;
    let mut inner = rt.inner.lock().await;
    let Some((hooks, started)) = inner
        .slots
        .get(&agent.name)
        .and_then(|s| s.turn.as_ref())
        .filter(|t| t.generation == generation)
        .map(|t| (t.hooks.clone(), t.started))
    else {
        return;
    };
    let set_phase = |inner: &mut Inner, phase: AgentPhase| {
        if let Some(slot) = inner.slots.get_mut(&agent.name) {
            slot.phase = phase;
        }
    };
    match update {
        TurnUpdate::Session(session) => {
            let record = record_mut(&mut inner, agent);
            record.set_session(&session);
            record.updated_at = Utc::now();
            rt.save(&inner);
            forward_unclaimed(context, &mut inner, agent, &session, &hooks, started);
        }
        TurnUpdate::NeedsYou(notice) => {
            set_phase(&mut inner, AgentPhase::NeedsYou);
            drop(inner);
            info!("agent '{}': needs you: {notice}", agent.name);
            if context.notify {
                send_notification_for(
                    "whisrs",
                    &format!("{}: {notice}", agent.name),
                    REPLY_NOTIFICATION_MS,
                );
            }
            rt.say(&agent.name, notice);
        }
        TurnUpdate::Working => set_phase(&mut inner, AgentPhase::Working),
    }
}

/// End the turn `generation`: persist, then speak and show the reply, or
/// notify the error.
async fn finish_turn(
    context: &DaemonContext,
    agent: &Agent,
    generation: u64,
    outcome: TurnOutcome,
) {
    let rt = &context.agents;
    {
        let mut inner = rt.inner.lock().await;
        let current = inner
            .slots
            .get(&agent.name)
            .and_then(|s| s.turn.as_ref())
            .is_some_and(|t| t.generation == generation);
        if !current {
            debug!("agent '{}': a replaced turn finished; ignored", agent.name);
            return;
        }
        if let Some(slot) = inner.slots.get_mut(&agent.name) {
            slot.turn = None;
            slot.phase = AgentPhase::Idle;
        }
        let record = record_mut(&mut inner, agent);
        record.turn_running = false;
        record.turn_started_at = None;
        record.updated_at = Utc::now();
        rt.save(&inner);
    }
    match outcome {
        TurnOutcome::Reply(raw) => {
            let text = spoken::spoken_reply(&raw);
            info!(
                "agent '{}': turn done ({} chars, {} spoken)",
                agent.name,
                raw.chars().count(),
                text.chars().count()
            );
            if context.notify {
                send_notification_for(
                    "whisrs",
                    &format!("{}: {text}", agent.name),
                    REPLY_NOTIFICATION_MS,
                );
            }
            rt.say(&agent.name, text);
        }
        TurnOutcome::Failed(message) => {
            // Driver messages name the agent already.
            error!("{message}");
            if context.notify_error() {
                send_notification_for("whisrs", &message, REPLY_NOTIFICATION_MS);
            }
        }
    }
}

/// Forget the agent's session. Refused while a turn runs.
async fn reset(context: &DaemonContext, agent: &Agent) -> Result<(), String> {
    let rt = &context.agents;
    let mut inner = rt.inner.lock().await;
    if inner
        .slots
        .get(&agent.name)
        .is_some_and(|s| s.turn.is_some())
    {
        return Err(format!(
            "{STILL_WORKING} Stop it first: whisrs agent {} --stop",
            agent.name
        ));
    }
    let record = record_mut(&mut inner, agent);
    if let Some(old) = record.session_id.take() {
        info!(
            "agent '{}': forgot session {old}; the next turn starts a new one (the old session \
             stays in Claude Code)",
            agent.name
        );
    }
    record.clear_session();
    record.updated_at = Utc::now();
    rt.save(&inner);
    Ok(())
}

/// Stop the running turn and keep the session.
async fn stop_turn(context: &DaemonContext, agent: &Agent) -> Result<(), String> {
    let rt = &context.agents;
    let session = {
        let mut inner = rt.inner.lock().await;
        let Some(turn) = inner.slots.get_mut(&agent.name).and_then(|s| s.turn.take()) else {
            return Err(format!("no turn is running for '{}'", agent.name));
        };
        turn.task.abort();
        if let Some(slot) = inner.slots.get_mut(&agent.name) {
            slot.phase = AgentPhase::Idle;
        }
        let record = record_mut(&mut inner, agent);
        record.turn_running = false;
        record.turn_started_at = None;
        record.updated_at = Utc::now();
        let session = record.session();
        rt.save(&inner);
        session
    };
    info!("agent '{}': turn stopped", agent.name);
    if let Err(e) = rt.driver(agent.harness).stop(agent, &session).await {
        warn!("agent '{}': {e}", agent.name);
        return Err(format!(
            "the turn was abandoned, but stopping the session failed: {e}"
        ));
    }
    Ok(())
}

/// Speak and notify the still-working refusal.
fn refuse_still_working(context: &DaemonContext, agent: &Agent) {
    info!("agent '{}': refused, a turn is running", agent.name);
    if context.notify_error() {
        send_notification("whisrs", &format!("{}: {STILL_WORKING}", agent.name));
    }
    context.agents.say(&agent.name, STILL_WORKING.to_string());
}

// ---------------------------------------------------------------------------
// Voice turns: the recording sink
// ---------------------------------------------------------------------------

/// `whisrs agent <name>` or its hotkey. Idle: record a voice turn. Recording
/// for this agent: stop and transcribe. Speaking: stop playback and record
/// (barge-in). Anything else: busy.
async fn voice_toggle(
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
    agent: Arc<Agent>,
) -> Response {
    {
        let mut ds = daemon_state.lock().await;
        match ds.state_machine.state() {
            State::Recording => {
                let ours = ds
                    .agent_recording
                    .as_ref()
                    .is_some_and(|r| r.name == agent.name);
                if !ours {
                    return error(
                        "busy: another recording is active. Finish or cancel it first \
                         (whisrs cancel)",
                    );
                }
                if let Some(mut capture) = ds.audio_capture.take() {
                    capture.stop();
                    tokio::task::spawn_blocking(move || drop(capture));
                }
                info!("agent '{}': manual stop", agent.name);
                return Response::Ok {
                    state: State::Recording,
                };
            }
            State::Transcribing => {
                return error("busy: still transcribing, try again in a moment");
            }
            State::Idle | State::Speaking | State::Synthesizing => {}
        }
    }

    if context.agents.phase(&agent.name).await == AgentPhase::Working {
        refuse_still_working(&context, &agent);
        return error(STILL_WORKING);
    }

    #[cfg(feature = "recording")]
    if let Some(refused) = refuse_while_recording(&context, DictationStart::Agent(&agent.name)) {
        return refused;
    }

    start_recording(daemon_state, context, agent).await
}

/// Start recording a voice turn. Under one lock, so a reply that would start
/// speaking cannot slip in between stopping playback and the recording.
async fn start_recording(
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
    agent: Arc<Agent>,
) -> Response {
    let audio_rx = {
        let mut ds = daemon_state.lock().await;
        match ds.state_machine.state() {
            State::Speaking | State::Synthesizing => {
                stop_read_aloud(&mut ds, &context);
                info!("agent '{}': barge-in, playback stopped", agent.name);
            }
            State::Idle => {}
            other => {
                return error(&format!("busy: the daemon is {other}"));
            }
        }

        if context.config.general.audio_feedback {
            feedback::play_start(context.config.general.audio_feedback_volume);
        }
        let mut capture = match AudioCaptureHandle::start_with_level_tx(
            &context.config.audio.device,
            context.overlay_level_tx.clone(),
        ) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("{e}");
                let friendly = if msg.contains("no default audio input device") {
                    format_no_microphone_error(&context.config.audio.device)
                } else {
                    format!("Failed to start audio capture: {e}")
                };
                error!("{friendly}");
                return error(&friendly);
            }
        };
        let audio_rx = capture.take_receiver();
        if let Err(e) = ds.state_machine.transition(Action::Toggle) {
            return error(&format!("state transition failed: {e}"));
        }
        ds.audio_capture = Some(capture);
        ds.recording_started_at = Some(std::time::Instant::now());
        ds.agent_recording = Some(AgentRecordingContext {
            name: agent.name.clone(),
        });
        let _ = context.state_tx.send(ds.state_machine.state());
        audio_rx
    };

    info!("agent '{}': recording a voice turn", agent.name);
    if context.notify_state() {
        send_notification(
            "whisrs",
            &format!("Speak to '{}'... (press again to stop)", agent.name),
        );
    }

    let ds_ref = Arc::clone(&daemon_state);
    let ctx = Arc::clone(&context);
    tokio::spawn(async move {
        voice_turn_background(audio_rx, ds_ref, ctx, agent).await;
    });

    Response::Ok {
        state: State::Recording,
    }
}

/// Collect the instruction, transcribe it, return the state machine to Idle,
/// then hand the transcript to the agent.
async fn voice_turn_background(
    audio_rx: Option<tokio::sync::mpsc::UnboundedReceiver<Vec<i16>>>,
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
    agent: Arc<Agent>,
) {
    let label = format!("agent '{}'", agent.name);
    let samples = collect_instruction_audio(audio_rx, &daemon_state, &context, &label).await;

    // Same claim as command mode: `whisrs cancel` (or a plain toggle taking
    // the recording over as dictation) takes the context under this lock,
    // and then the recording is discarded.
    let claimed = {
        let mut guard = daemon_state.lock().await;
        let ds = &mut *guard;
        let claimed = claim_session(&mut ds.state_machine, &mut ds.agent_recording);
        if claimed.is_some() {
            ds.audio_capture.take();
            ds.recording_started_at.take();
            let _ = context.state_tx.send(ds.state_machine.state());
        }
        claimed
    };
    if claimed.is_none() {
        info!("{label}: voice turn cancelled, recording discarded");
        return;
    }

    if context.config.general.audio_feedback {
        feedback::play_stop(context.config.general.audio_feedback_volume);
    }
    let transcript = transcribe_batch_audio(
        &samples,
        &context,
        &context.config.general.language,
        &BatchOptions::agent(&agent.name),
    )
    .await;

    // Back to Idle at once: the turn runs on its own task.
    {
        let mut ds = daemon_state.lock().await;
        let _ = ds.state_machine.transition(Action::TranscriptionDone);
        let _ = context.state_tx.send(ds.state_machine.state());
        if let Some(level_tx) = &context.overlay_level_tx {
            let _ = level_tx.send(0.0);
        }
    }

    let text = match transcript {
        Ok(text) => text,
        Err(e) => {
            let friendly = format_api_error(&e);
            error!("{label}: transcription failed: {e:#}");
            if context.notify_error() {
                send_notification(
                    "whisrs",
                    &format!("Agent '{}': transcription failed: {friendly}", agent.name),
                );
            }
            return;
        }
    };
    if text.trim().is_empty() {
        // Gated or unintelligible: the helper already notified.
        return;
    }
    if let Err(message) = start_turn(&context, &agent, text).await {
        if message == STILL_WORKING {
            refuse_still_working(&context, &agent);
        } else if context.notify_error() {
            send_notification("whisrs", &message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_still_working_refusal_is_not_notified_twice() {
        assert!(error_already_notified(STILL_WORKING));
        assert!(error_already_notified(&format!(
            "{STILL_WORKING} Stop it first: whisrs agent jarvis --stop"
        )));
        assert!(error_already_notified(
            "A recording is in progress. Agent 'jarvis' is off until you stop it."
        ));
        assert!(!error_already_notified("no agent named 'x'"));
    }

    #[test]
    fn no_agents_means_an_inert_runtime() {
        let config: Config = toml::from_str("").unwrap();
        let rt = AgentRuntime::new(&config, Path::new("/nonexistent.sock"));
        assert!(!rt.is_enabled());
        assert!(!rt.has_hotkeys());
        assert!(rt.agents().is_empty());
        assert!(rt.drivers.is_empty());
        // Nothing is queued to speak.
        rt.say("jarvis", "x".into());
        assert!(!rt.has_utterances());
    }

    /// A session's events from before a turn started (its previous turn, or
    /// a follow-up it ran on its own) never end the new turn; events that
    /// arrived after the start (a first turn's Stop, before its session id
    /// was known) do. Other sessions' events stay.
    #[test]
    fn early_events_reach_the_turn_and_stale_ones_are_dropped() {
        let t0 = Instant::now();
        let start = t0 + Duration::from_secs(5);
        let ev = |sid: &str, n: u32| serde_json::json!({"session_id": sid, "n": n});
        let mut events: VecDeque<_> = [
            (t0, ev("ours", 1)),
            (t0, ev("other", 2)),
            (start, ev("ours", 3)),
            (start + Duration::from_secs(1), ev("ours", 4)),
        ]
        .into_iter()
        .collect();
        let (fresh, stale) = take_session_events(&mut events, |p| p["session_id"] == "ours", start);
        assert_eq!(fresh, vec![ev("ours", 3), ev("ours", 4)]);
        assert_eq!(stale, 1);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].1, ev("other", 2));
    }

    #[test]
    fn utterances_are_spoken_in_order_and_requeued_at_the_front() {
        let mut rt = AgentRuntime::disabled();
        rt.speak = true;
        rt.say("a", "one".into());
        rt.say("b", "two".into());
        let first = rt.next_utterance().unwrap();
        assert_eq!(first.text, "one");
        rt.requeue_front(first);
        assert_eq!(rt.next_utterance().unwrap().text, "one");
        assert_eq!(rt.next_utterance().unwrap().text, "two");
        assert!(rt.next_utterance().is_none());
    }
}
