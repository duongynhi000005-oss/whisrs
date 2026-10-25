//! Agent mode runtime (`[[agents]]`): per-agent slots, running turns, the
//! reply queue, the IPC handlers and the hook events.
//!
//! Lives in [`DaemonContext`] behind its own lock, outside [`DaemonState`]:
//! the dictation state machine only gains the agent recording sink (a voice
//! turn records exactly like command mode's instruction, then hands the
//! transcript here). A turn runs on its own task, so the state machine is
//! back to Idle as soon as the transcript is ready and dictation keeps
//! working while agents run. Replies wait in a FIFO and are spoken when the
//! daemon is Idle and no recording captures (its system track would pick
//! them up). A reply the session produces on its own, with no whisrs turn
//! running (a background command finished and the agent went on), is spoken
//! the same way.
//!
//! The runtime is harness-neutral: it calls [`Driver`] and never names a
//! harness outside [`AgentRuntime::new`].
//!
//! With no usable `[[agents]]` entry nothing here runs: no task, no process,
//! no file read or written.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use chrono::Utc;
use tokio::sync::{mpsc, Mutex, Notify};
use tracing::{debug, error, info, warn};

use whisrs::agent::claude::ClaudeDriver;
use whisrs::agent::store::{self, AgentRecord, AgentStore};
use whisrs::agent::{
    resolve_agents, spoken, Agent, AgentAction, AgentInfo, AgentPhase, Driver, Harness, HookEvent,
    IdleEvent, SessionRef, TurnIo, TurnOutcome, TurnRequest, TurnUpdate, TurnUpdates,
    BACKGROUND_TASK, STILL_WORKING,
};
use whisrs::audio::capture::AudioCaptureHandle;
use whisrs::audio::feedback;
use whisrs::state::Action;
use whisrs::{Config, Response, State};

use crate::command_mode::{claim_session, collect_instruction_audio};
use crate::context::{AgentRecordingContext, DaemonContext, DaemonState};
use crate::notify::{send_notification, send_notification_for, truncate_preview};
use crate::pipeline::{
    format_api_error, format_no_microphone_error, transcribe_batch_audio, BatchOptions,
};
#[cfg(feature = "recording")]
use crate::recording::{refuse_while_recording, DictationStart};
use crate::speak::{speak_text, stop_read_aloud, tts_backend, SpeakAttempt, SpeakOrigin};

/// How long a hook event no turn claimed is kept: a first turn's `Stop` can
/// arrive before `claude agents` has told us its session id.
const UNCLAIMED_TTL: Duration = Duration::from_secs(120);

/// At most this many unclaimed hook events are kept.
const UNCLAIMED_MAX: usize = 32;

/// How long a reply notification stays up.
const REPLY_NOTIFICATION_MS: u32 = 10_000;

/// Spoken when a voice turn's audio held nothing to send (too short,
/// silent, an echo of the prompt, or an empty transcript).
const DIDNT_CATCH: &str = "I didn't catch that.";

/// The longest instruction a refusal notification repeats, in bytes.
const REFUSED_INSTRUCTION_MAX: usize = 500;

/// Whether an agent error was already notified where it happened: the
/// busy refusals, and the refusal while a recording runs
/// (`refuse_while_recording` notifies it).
pub(crate) fn error_already_notified(message: &str) -> bool {
    message.starts_with(STILL_WORKING)
        || message.starts_with(BACKGROUND_TASK)
        || message.starts_with("A recording is in progress.")
}

/// Why a new turn was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Busy {
    /// The agent's whisrs turn is still running.
    Turn,
    /// Its session works on its own, with no whisrs turn running (a
    /// background task, or a follow-up after one). Stopping that work for a
    /// new turn would lose it, so `whisrs agent <name> --stop` is the way on.
    Background,
}

impl Busy {
    /// The spoken notice.
    fn notice(self) -> &'static str {
        match self {
            Busy::Turn => STILL_WORKING,
            Busy::Background => BACKGROUND_TASK,
        }
    }

    /// The error a CLI request gets back.
    fn message(self, agent: &str) -> String {
        match self {
            Busy::Turn => STILL_WORKING.to_string(),
            Busy::Background => format!(
                "{BACKGROUND_TASK} `whisrs agent {agent} --stop` ends that task, then send it again."
            ),
        }
    }
}

/// The notification for a refused turn. With the instruction (it was
/// transcribed or typed, and is not sent), it repeats it so the user can
/// send it again.
fn busy_notification(agent: &str, busy: Busy, instruction: Option<&str>) -> String {
    let mut body = format!("{agent}: {}", busy.notice());
    if let Some(text) = instruction.map(str::trim).filter(|t| !t.is_empty()) {
        body.push_str(&format!(
            " Not sent: \"{}\".",
            truncate_preview(text, REFUSED_INSTRUCTION_MAX)
        ));
    }
    if busy == Busy::Background {
        body.push_str(&format!(
            " `whisrs agent {agent} --stop` ends that task, then send it again."
        ));
    }
    body
}

/// Two raw replies that are the same message: equal up to whitespace (the
/// transcript's text of a turn that ended on a poll can differ from the
/// Stop hook's in whitespace).
fn same_reply(a: &str, b: &str) -> bool {
    a.split_whitespace().eq(b.split_whitespace())
}

/// One running turn.
struct RunningTurn {
    /// Tells a finished task from the one that replaced it.
    generation: u64,
    /// When it started: only hook events that arrived later can be its own.
    started: Instant,
    task: tokio::task::JoinHandle<()>,
    /// The instruction it sends, for the notification when it is refused.
    text: String,
    /// Hook events for the driver.
    hooks: mpsc::UnboundedSender<HookEvent>,
    /// The newest session ids the turn reported, and its stop flag
    /// ([`TurnUpdates::stop`]).
    updates: TurnUpdates,
}

/// What the runtime holds per agent at run time (the persisted part is the
/// agent's [`AgentRecord`]).
struct Slot {
    phase: AgentPhase,
    turn: Option<RunningTurn>,
    /// The raw reply of the last turn, when it ended on a poll: that turn's
    /// own `Stop` can still arrive after it, carrying the same reply, and is
    /// not spoken twice. Taken by the next `Stop` with no turn running, and
    /// cleared when another turn ends.
    polled_reply: Option<String>,
}

impl Default for Slot {
    fn default() -> Self {
        Self {
            phase: AgentPhase::Idle,
            turn: None,
            polled_reply: None,
        }
    }
}

struct Inner {
    store: AgentStore,
    slots: HashMap<String, Slot>,
    /// Hook events no running turn matched yet.
    unclaimed: VecDeque<HookEvent>,
    next_generation: u64,
}

/// One queued utterance.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Utterance {
    agent: String,
    text: String,
    /// The busy notice this speaks ([`Busy::notice`]): one per agent at a
    /// time, queued or playing. `None` for replies and every other line,
    /// which are never deduplicated.
    notice: Option<&'static str>,
}

/// Holds agent replies while it lives: a command-mode or `[[llm_commands]]`
/// press between its state check and its recording, or a read-aloud press
/// until its read-aloud started (the selection capture releases the
/// dictation lock), or a recording that is starting. Dropping it wakes the
/// reply queue.
pub(crate) struct ReplyHold<'a> {
    rt: &'a AgentRuntime,
}

impl Drop for ReplyHold<'_> {
    fn drop(&mut self) {
        self.rt.reply_holds.fetch_sub(1, Ordering::SeqCst);
        self.rt.utterance_ready.notify_one();
    }
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
    /// The utterance being spoken, until its playback ends.
    playing: std::sync::Mutex<Option<Utterance>>,
    /// TTS is enabled: utterances are queued at all.
    speak: bool,
    /// Live [`ReplyHold`]s.
    reply_holds: AtomicUsize,
    /// The dictation state, set once the reply queue starts: a recording
    /// start stops an agent reply playing through it.
    daemon_state: OnceLock<Arc<Mutex<DaemonState>>>,
    /// Numbers `agents.json` snapshots; the newest written one is kept in
    /// `saved_seq`, so a write that lost the race to a newer one is skipped.
    save_seq: AtomicU64,
    saved_seq: Arc<std::sync::Mutex<u64>>,
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
            playing: std::sync::Mutex::new(None),
            speak: config.tts.as_ref().is_some_and(|t| t.enabled),
            reply_holds: AtomicUsize::new(0),
            daemon_state: OnceLock::new(),
            save_seq: AtomicU64::new(0),
            saved_seq: Arc::new(std::sync::Mutex::new(0)),
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
            playing: std::sync::Mutex::new(None),
            speak: false,
            reply_holds: AtomicUsize::new(0),
            daemon_state: OnceLock::new(),
            save_seq: AtomicU64::new(0),
            saved_seq: Arc::new(std::sync::Mutex::new(0)),
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

    /// Whether any agent has a hotkey (`hotkey` or `new_session_hotkey`).
    pub(crate) fn has_hotkeys(&self) -> bool {
        self.agents
            .iter()
            .any(|a| a.hotkey.is_some() || a.new_session_hotkey.is_some())
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

    /// Write a snapshot of the store off the async runtime (the write
    /// fsyncs). Snapshots are numbered and a write older than the newest
    /// one on disk is skipped, so the file always ends at the newest state.
    fn save(&self, inner: &Inner) {
        let seq = self.save_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let snapshot = inner.store.clone();
        let path = self.store_path.clone();
        let saved = Arc::clone(&self.saved_seq);
        tokio::task::spawn_blocking(move || {
            let mut last = saved.lock().unwrap_or_else(|e| e.into_inner());
            if *last >= seq {
                return;
            }
            match store::save(&path, &snapshot) {
                Ok(()) => *last = seq,
                Err(e) => warn!("agents: cannot write {}: {e:#}", path.display()),
            }
        });
    }

    /// Queue `text` to be spoken when the daemon is Idle (TTS on only).
    fn say(&self, agent: &str, text: String) {
        self.enqueue(Utterance {
            agent: agent.to_string(),
            text,
            notice: None,
        });
    }

    /// Queue a busy notice, named after the agent when there are several.
    /// Not queued again while the same notice for this agent is queued or
    /// playing: compared as the notice, not the spoken line.
    fn say_notice(&self, agent: &str, notice: &'static str) {
        self.enqueue(Utterance {
            agent: agent.to_string(),
            text: self.line_for(agent, notice.to_string()),
            notice: Some(notice),
        });
    }

    fn enqueue(&self, utterance: Utterance) {
        if !self.speak {
            return;
        }
        if let Ok(mut queue) = self.utterances.lock() {
            let same_notice = |u: &Utterance| {
                utterance.notice.is_some()
                    && u.agent == utterance.agent
                    && u.notice == utterance.notice
            };
            let playing = self
                .playing
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(same_notice);
            if playing || queue.iter().any(same_notice) {
                debug!(
                    "agent '{}': that notice is already queued or playing",
                    utterance.agent
                );
                return;
            }
            queue.push_back(utterance);
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

    /// Run `stop`, which stops an agent reply or notice being synthesized or
    /// played (the caller holds the dictation lock), and when it stopped
    /// one, put that utterance back at the front of the queue, so it plays
    /// after the recording. `true` when one was stopped.
    ///
    /// The utterance is copied before `stop` runs: the stop broadcasts Idle,
    /// and the reply queue, woken by it on another worker, clears `playing`
    /// ([`utterance_loop`]) perhaps before `stop` returns. The queue only
    /// clears it and never puts back a reply that ran, so the copy is
    /// queued exactly once.
    #[cfg(feature = "recording")]
    fn requeue_if_stopped(&self, stop: impl FnOnce() -> bool) -> bool {
        let playing = self
            .playing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if !stop() {
            return false;
        }
        if let Some(utterance) = playing {
            info!(
                "agent '{}': the reply plays again after the recording",
                utterance.agent
            );
            self.requeue_front(utterance);
        }
        true
    }

    /// Hold agent replies until the guard drops ([`ReplyHold`]).
    pub(crate) fn hold_replies(&self) -> ReplyHold<'_> {
        self.reply_holds.fetch_add(1, Ordering::SeqCst);
        ReplyHold { rt: self }
    }

    fn has_utterances(&self) -> bool {
        self.utterances.lock().is_ok_and(|q| !q.is_empty())
    }

    fn set_playing(&self, utterance: Option<Utterance>) {
        *self.playing.lock().unwrap_or_else(|e| e.into_inner()) = utterance;
    }

    /// Whether the queue is speaking an utterance (from just before it asks
    /// for the dictation state until its playback ended).
    #[cfg(feature = "recording")]
    fn is_speaking(&self) -> bool {
        self.playing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// Drop every queued reply and notice (`whisrs cancel` while one plays).
    pub(crate) fn drop_queued(&self) {
        let dropped = self
            .utterances
            .lock()
            .map(|mut q| std::mem::take(&mut *q).len())
            .unwrap_or(0);
        if dropped > 0 {
            info!("agents: cancel dropped {dropped} queued reply(s)");
        }
    }

    /// The spoken line for a failed turn, naming the agent when there are
    /// several.
    fn error_line(&self, agent: &Agent, message: &str) -> String {
        self.line_for(&agent.name, spoken::spoken_error(message))
    }

    /// `line` named after `agent` when there are several agents.
    fn line_for(&self, agent: &str, line: String) -> String {
        if self.agents.len() > 1 {
            format!("{agent}: {line}")
        } else {
            line
        }
    }

    /// The row `whisrs agent` shows for `agent`.
    fn info(inner: &Inner, agent: &Agent) -> AgentInfo {
        let phase = inner
            .slots
            .get(&agent.name)
            .map(|s| s.phase)
            .unwrap_or(AgentPhase::Idle);
        let record = inner.store.agents.get(&agent.name);
        let session = record
            .and_then(|r| r.session_id.as_deref())
            .map(|id| id.chars().take(8).collect());
        AgentInfo {
            name: agent.name.clone(),
            harness: agent.harness.to_string(),
            phase,
            session,
            bg_id: record.and_then(|r| r.bg_id.clone()),
        }
    }

    /// The session of `agent` when no whisrs turn runs and it has one: what
    /// [`Driver::session_phase`] is asked about.
    fn idle_session(inner: &Inner, agent: &Agent) -> Option<SessionRef> {
        if inner
            .slots
            .get(&agent.name)
            .is_some_and(|s| s.turn.is_some())
        {
            return None;
        }
        let session = inner.store.agents.get(&agent.name)?.session();
        session.bg_id.is_some().then_some(session)
    }

    /// Every agent's row. An agent with no whisrs turn running is shown as
    /// its harness sees it, so a session working on its own reads `working`.
    /// The harness is asked once for every such agent
    /// ([`Driver::session_phases`]).
    async fn list(&self) -> Vec<AgentInfo> {
        let (mut rows, probes) = {
            let inner = self.inner.lock().await;
            let rows: Vec<AgentInfo> = self.agents.iter().map(|a| Self::info(&inner, a)).collect();
            let probes: Vec<(usize, Arc<Agent>, SessionRef)> = self
                .agents
                .iter()
                .enumerate()
                .filter_map(|(i, a)| Self::idle_session(&inner, a).map(|s| (i, Arc::clone(a), s)))
                .collect();
            (rows, probes)
        };
        for harness in Harness::ALL {
            let group: Vec<&(usize, Arc<Agent>, SessionRef)> =
                probes.iter().filter(|p| p.1.harness == *harness).collect();
            if group.is_empty() {
                continue;
            }
            let sessions: Vec<(Arc<Agent>, SessionRef)> = group
                .iter()
                .map(|(_, agent, session)| (Arc::clone(agent), session.clone()))
                .collect();
            let phases = self.driver(*harness).session_phases(&sessions).await;
            for ((i, agent, _), phase) in group.into_iter().zip(phases) {
                match phase {
                    Ok(phase) => rows[*i].phase = phase,
                    Err(e) => debug!(
                        "agent '{}': cannot tell what its session does: {e}",
                        agent.name
                    ),
                }
            }
        }
        rows
    }

    async fn phase(&self, name: &str) -> AgentPhase {
        let inner = self.inner.lock().await;
        inner
            .slots
            .get(name)
            .map(|s| s.phase)
            .unwrap_or(AgentPhase::Idle)
    }

    /// Whether a whisrs turn of the agent runs, also one that waits for the
    /// user (needs-you).
    async fn has_turn(&self, name: &str) -> bool {
        let inner = self.inner.lock().await;
        inner.slots.get(name).is_some_and(|s| s.turn.is_some())
    }
}

/// A session for the log: its id, else the bg id of a launch whose session
/// id was not looked up yet, else none.
fn describe(session: &SessionRef) -> String {
    match (&session.session_id, &session.bg_id) {
        (Some(id), _) => format!("session {id}"),
        (None, Some(bg)) => format!("session bg {bg}"),
        (None, None) => "a new session".to_string(),
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
    let _ = context.agents.daemon_state.set(Arc::clone(&daemon_state));
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
            "agent '{}': a turn was running before the restart; watching {} again",
            agent.name,
            describe(&session)
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

/// Whether a queued reply or notice may be spoken now: the daemon is Idle
/// (no dictation, command or read-aloud) and nothing else holds replies
/// ([`replies_held`]).
fn may_speak(state: State, held: bool) -> bool {
    state == State::Idle && !held
}

/// Whether agent replies wait besides the dictation state: a
/// [`ReplyHold`] (a command or read-aloud press capturing the selection, a
/// recording starting), or a recording capturing (a meeting recording does not touch
/// the dictation state, and a reply would play mid-call and land on its
/// system track). Checked under the dictation lock by `speak_text`, so a
/// hold taken under that lock is never missed.
pub(crate) fn replies_held(context: &DaemonContext) -> bool {
    context.agents.reply_holds.load(Ordering::SeqCst) > 0 || recording_captures(context)
}

fn recording_captures(_context: &DaemonContext) -> bool {
    #[cfg(feature = "recording")]
    return _context.recording.is_capturing();
    #[cfg(not(feature = "recording"))]
    false
}

/// A recording is starting: hold agent replies until the returned guard
/// drops (by then the recording captures, or its start failed), and stop an
/// agent reply or notice being synthesized or played, putting it back at
/// the front of the queue so it plays after the recording. A selection
/// read-aloud is left alone.
///
/// Never waits on the dictation lock, which a dictation start holds for
/// seconds while its mic opens: a recording start must not wait on it. The
/// hold is taken first, so a reply the queue starts after it is refused
/// under that lock (`speak_text`), and only one that already started needs
/// stopping. When the lock is busy, that stop is spawned and runs once the
/// lock is free, while replies are still held.
#[cfg(feature = "recording")]
pub(crate) fn hold_for_recording(context: &Arc<DaemonContext>) -> ReplyHold<'_> {
    let rt = &context.agents;
    let hold = rt.hold_replies();
    if !rt.is_speaking() {
        return hold;
    }
    let Some(daemon_state) = rt.daemon_state.get() else {
        return hold;
    };
    match daemon_state.try_lock() {
        Ok(mut ds) => stop_for_recording(&mut ds, context),
        Err(_) => {
            debug!(
                "recording: the dictation lock is busy; an agent reply is stopped once it is free"
            );
            let daemon_state = Arc::clone(daemon_state);
            let context = Arc::clone(context);
            tokio::spawn(async move {
                let mut ds = daemon_state.lock().await;
                // Nothing holds replies any more: the recording start
                // failed and is gone, so there is nothing to stop for.
                if replies_held(&context) {
                    stop_for_recording(&mut ds, &context);
                }
            });
        }
    }
    hold
}

/// Stop an agent reply or notice playing for a recording start and put it
/// back at the front of the queue. The caller holds the dictation lock.
#[cfg(feature = "recording")]
fn stop_for_recording(ds: &mut DaemonState, context: &DaemonContext) {
    context
        .agents
        .requeue_if_stopped(|| crate::speak::stop_agent_playback(ds, context, "recording"));
}

/// What wakes the reply queue besides the dictation state: the recording
/// status (feature `recording`).
struct RecordingHold {
    #[cfg(feature = "recording")]
    status_rx: tokio::sync::watch::Receiver<whisrs::recording::RecordingStatus>,
}

impl RecordingHold {
    fn new(_context: &DaemonContext) -> Self {
        Self {
            #[cfg(feature = "recording")]
            status_rx: _context.recording.subscribe(),
        }
    }

    /// Resolves when the recording status may have changed (never without
    /// the feature).
    async fn changed(&mut self) {
        #[cfg(feature = "recording")]
        if self.status_rx.changed().await.is_ok() {
            return;
        }
        std::future::pending::<()>().await
    }
}

/// Wait until the dictation state, the recording status or a hold may have
/// changed (a dropped [`ReplyHold`] wakes `utterance_ready`). `false` once
/// the state channel closed.
async fn wait_for_change(
    context: &DaemonContext,
    state_rx: &mut tokio::sync::watch::Receiver<State>,
    hold: &mut RecordingHold,
) -> bool {
    tokio::select! {
        changed = state_rx.changed() => changed.is_ok(),
        _ = hold.changed() => true,
        _ = context.agents.utterance_ready.notified() => true,
    }
}

/// Speak queued replies and notices one at a time, each once
/// [`may_speak`]. Watches the state channel like `hook_dispatch_loop`, and
/// the recording status.
async fn utterance_loop(
    context: Arc<DaemonContext>,
    daemon_state: Arc<Mutex<DaemonState>>,
    mut state_rx: tokio::sync::watch::Receiver<State>,
) {
    let mut hold = RecordingHold::new(&context);
    loop {
        if !context.agents.has_utterances() {
            context.agents.utterance_ready.notified().await;
            continue;
        }
        let state = *state_rx.borrow_and_update();
        let held = replies_held(&context);
        if !may_speak(state, held) {
            if held {
                debug!("agents: a recording or a command or read-aloud press holds replies");
            }
            if !wait_for_change(&context, &mut state_rx, &mut hold).await {
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
        context.agents.set_playing(Some(utterance.clone()));
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
                // Something started first, or a hold came in: wait again.
                context.agents.set_playing(None);
                context.agents.requeue_front(utterance);
                if !wait_for_change(&context, &mut state_rx, &mut hold).await {
                    return;
                }
            }
            SpeakAttempt::Ran(Response::Error { message }) => {
                context.agents.set_playing(None);
                warn!("agent '{}': reply not spoken: {message}", utterance.agent);
            }
            SpeakAttempt::Ran(_) => {
                // It counts as playing until the playback ends.
                let ended = state_rx
                    .wait_for(|s| !matches!(s, State::Speaking | State::Synthesizing))
                    .await
                    .is_ok();
                context.agents.set_playing(None);
                if !ended {
                    return;
                }
            }
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
        AgentAction::Toggle => voice_toggle(daemon_state, context, agent, new).await,
        AgentAction::Send => {
            let Some(text) = text.filter(|t| !t.trim().is_empty()) else {
                return error("nothing to send: the text is empty");
            };
            if new {
                if let Err(message) = reset(&context, &agent).await {
                    return error(&message);
                }
            }
            let text = text.trim().to_string();
            match start_turn(&context, &agent, text.clone()).await {
                Ok(()) => agent_row(&context, &agent).await,
                Err(busy) => error(&refuse_busy(&context, &agent, busy, Some(&text))),
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

/// Who a hook event belongs to.
enum EventOwner {
    /// The running turn of this agent.
    Turn(String, mpsc::UnboundedSender<HookEvent>),
    /// This agent's stored session, with no whisrs turn running.
    Idle(Arc<Agent>, SessionRef),
}

/// `{"cmd":"agent-event"}`: route a hook event to the running turn whose
/// stored session it belongs to; with no turn running, speak the reply a
/// session produced on its own, or announce the permission prompt its own
/// work waits on; or keep it briefly for a turn that does not know its
/// session id yet.
pub(crate) async fn handle_agent_event(context: &DaemonContext, payload: serde_json::Value) {
    let rt = &context.agents;
    if !rt.is_enabled() {
        debug!("agent-event ignored: no agents configured");
        return;
    }
    let event = HookEvent::now(payload);
    let mut inner = rt.inner.lock().await;
    let mut owner = None;
    for agent in &rt.agents {
        let Some(record) = inner.store.agents.get(&agent.name) else {
            continue;
        };
        if !rt
            .driver(agent.harness)
            .hook_matches(&event.payload, &record.session())
        {
            continue;
        }
        owner = Some(
            match inner.slots.get(&agent.name).and_then(|s| s.turn.as_ref()) {
                Some(turn) => EventOwner::Turn(agent.name.clone(), turn.hooks.clone()),
                None => EventOwner::Idle(Arc::clone(agent), record.session()),
            },
        );
        break;
    }
    match owner {
        Some(EventOwner::Turn(name, hooks)) => {
            debug!("agent '{name}': hook event routed");
            let _ = hooks.send(event);
        }
        Some(EventOwner::Idle(agent, session)) => {
            drop(inner);
            match rt
                .driver(agent.harness)
                .idle_event(&event.payload, &session)
                .await
            {
                IdleEvent::Reply(raw) => own_reply(context, &agent, raw).await,
                IdleEvent::NeedsYou(notice) => {
                    {
                        let mut inner = rt.inner.lock().await;
                        let slot = inner.slots.entry(agent.name.clone()).or_default();
                        if slot.turn.is_none() {
                            slot.phase = AgentPhase::NeedsYou;
                        }
                    }
                    info!(
                        "agent '{}': work its session does on its own waits for you",
                        agent.name
                    );
                    announce_needs_you(context, &agent, notice);
                }
                IdleEvent::Ignored => debug!(
                    "agent '{}': hook event with no turn running ignored",
                    agent.name
                ),
            }
        }
        None => {
            let now = event.received;
            inner
                .unclaimed
                .retain(|e| now.duration_since(e.received) < UNCLAIMED_TTL);
            if inner.unclaimed.len() >= UNCLAIMED_MAX {
                inner.unclaimed.pop_front();
            }
            debug!("agent-event: no running turn matches yet; kept for a moment");
            inner.unclaimed.push_back(event);
        }
    }
}

/// A reply the session produced on its own, with no whisrs turn running it
/// (a `Stop` with no turn running, or the old process's `Stop` before a
/// resumed turn's launch): spoken like a reply, unless it is the late `Stop`
/// of a turn that already ended on a poll, which carries that turn's reply
/// again.
async fn own_reply(context: &DaemonContext, agent: &Agent, raw: String) {
    {
        let mut inner = context.agents.inner.lock().await;
        let slot = inner.slots.entry(agent.name.clone()).or_default();
        if slot
            .polled_reply
            .take()
            .is_some_and(|polled| same_reply(&polled, &raw))
        {
            debug!(
                "agent '{}': the Stop of a turn that already ended on a poll; not spoken twice",
                agent.name
            );
            return;
        }
        if slot.turn.is_none() && slot.phase == AgentPhase::NeedsYou {
            slot.phase = AgentPhase::Idle;
        }
    }
    info!("agent '{}': the session replied on its own", agent.name);
    deliver_reply(context, agent, &raw);
}

/// Speak and notify a needs-you notice ("Waiting for your approval to use
/// Bash.").
fn announce_needs_you(context: &DaemonContext, agent: &Agent, notice: String) {
    info!("agent '{}': needs you: {notice}", agent.name);
    if context.notify {
        send_notification_for(
            "whisrs",
            &format!("{}: {notice}", agent.name),
            REPLY_NOTIFICATION_MS,
        );
    }
    let rt = &context.agents;
    rt.say(&agent.name, rt.line_for(&agent.name, notice));
}

/// Speak and show an agent's reply (raw final message).
fn deliver_reply(context: &DaemonContext, agent: &Agent, raw: &str) {
    let text = spoken::spoken_reply(raw);
    info!(
        "agent '{}': reply ({} chars, {} spoken)",
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
    context.agents.say(&agent.name, text);
}

// ---------------------------------------------------------------------------
// Turns
// ---------------------------------------------------------------------------

/// Start a turn for `agent`. Refused while its turn is working
/// ([`Busy::Turn`]), and while its session works on its own outside a whisrs
/// turn ([`Busy::Background`]: stopping it would lose that work); a turn
/// that waits for the user (needs-you) is replaced, so answering it by voice
/// is one press.
async fn start_turn(
    context: &Arc<DaemonContext>,
    agent: &Arc<Agent>,
    text: String,
) -> Result<(), Busy> {
    let rt = &context.agents;
    let idle_session = {
        let inner = rt.inner.lock().await;
        if let Some(slot) = inner.slots.get(&agent.name) {
            if slot.turn.is_some() && slot.phase != AgentPhase::NeedsYou {
                return Err(Busy::Turn);
            }
        }
        AgentRuntime::idle_session(&inner, agent)
    };
    if let Some(session) = idle_session {
        match rt
            .driver(agent.harness)
            .session_phase(agent, &session)
            .await
        {
            Ok(AgentPhase::Working) => {
                info!(
                    "agent '{}': its session is working on its own; not stopped for a new turn",
                    agent.name
                );
                return Err(Busy::Background);
            }
            Ok(_) => {}
            Err(e) => debug!(
                "agent '{}': cannot tell what its session does: {e}",
                agent.name
            ),
        }
    }
    let mut inner = rt.inner.lock().await;
    let slot = inner.slots.entry(agent.name.clone()).or_default();
    let mut replaced = None;
    if slot.turn.is_some() {
        if slot.phase != AgentPhase::NeedsYou {
            return Err(Busy::Turn);
        }
        info!(
            "agent '{}': a new turn answers the one waiting for you",
            agent.name
        );
        if let Some(turn) = slot.turn.take() {
            replaced = turn.updates.stop();
            turn.task.abort();
        }
    }
    let now = Utc::now();
    let record = record_mut(&mut inner, agent);
    if let Some(latest) = replaced {
        record.set_session(&latest);
    }
    record.turn_running = true;
    record.turn_started_at = Some(now);
    record.updated_at = now;
    let session = record.session();
    rt.save(&inner);
    info!(
        "agent '{}': starting a turn ({} chars) in {}",
        agent.name,
        text.chars().count(),
        describe(&session)
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
    let (updates, updates_rx) = TurnUpdates::channel();
    let started = Instant::now();
    // Events this session sent before the turn started belong to an earlier
    // turn (or one the session ran on its own, like a follow-up after a
    // background command): drop them so they cannot end this one.
    forward_unclaimed(context, inner, &agent, &session, &hooks_tx, started);
    let ctx = Arc::clone(context);
    let task_agent = Arc::clone(&agent);
    let io = TurnIo {
        hooks: hooks_rx,
        updates: updates.clone(),
    };
    let text = request.text.clone();
    let task = tokio::spawn(async move {
        run_turn_task(
            ctx, task_agent, generation, session, request, io, updates_rx, rewatch,
        )
        .await;
    });
    let slot = inner.slots.entry(agent.name.clone()).or_default();
    slot.phase = AgentPhase::Working;
    slot.turn = Some(RunningTurn {
        generation,
        started,
        task,
        text,
        hooks: hooks_tx,
        updates,
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
    hooks: &mpsc::UnboundedSender<HookEvent>,
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
    for event in fresh {
        debug!("agent '{}': delivering an early hook event", agent.name);
        let _ = hooks.send(event);
    }
}

/// Remove the events `belongs` accepts from `events`: those that arrived at
/// or after `since` are returned in order (with when they arrived, for the
/// driver), older ones are counted and dropped. Everything else stays.
fn take_session_events(
    events: &mut VecDeque<HookEvent>,
    belongs: impl Fn(&serde_json::Value) -> bool,
    since: Instant,
) -> (Vec<HookEvent>, usize) {
    let mut fresh = Vec::new();
    let mut stale = 0;
    let mut kept = VecDeque::new();
    for event in events.drain(..) {
        if !belongs(&event.payload) {
            kept.push_back(event);
        } else if event.received >= since {
            fresh.push(event);
        } else {
            stale += 1;
        }
    }
    *events = kept;
    (fresh, stale)
}

#[allow(clippy::too_many_arguments)]
async fn run_turn_task(
    context: Arc<DaemonContext>,
    agent: Arc<Agent>,
    generation: u64,
    session: SessionRef,
    request: TurnRequest,
    io: TurnIo,
    mut updates_rx: mpsc::UnboundedReceiver<TurnUpdate>,
    rewatch: bool,
) {
    let driver = context.agents.driver(agent.harness);
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
            announce_needs_you(context, agent, notice);
        }
        TurnUpdate::Working => set_phase(&mut inner, AgentPhase::Working),
        TurnUpdate::SessionReply(raw) => {
            drop(inner);
            own_reply(context, agent, raw).await;
        }
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
    let text = {
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
        let mut text = String::new();
        if let Some(slot) = inner.slots.get_mut(&agent.name) {
            text = slot.turn.take().map(|t| t.text).unwrap_or_default();
            slot.phase = AgentPhase::Idle;
            // Armed only by a turn that ended on a poll: its own late Stop
            // is the one duplicate to expect.
            slot.polled_reply = match &outcome {
                TurnOutcome::PolledReply(raw) => Some(raw.clone()),
                _ => None,
            };
        }
        let record = record_mut(&mut inner, agent);
        record.turn_running = false;
        record.turn_started_at = None;
        record.updated_at = Utc::now();
        rt.save(&inner);
        text
    };
    match outcome {
        TurnOutcome::Reply(raw) | TurnOutcome::PolledReply(raw) => {
            info!("agent '{}': turn done", agent.name);
            deliver_reply(context, agent, &raw);
        }
        TurnOutcome::Busy => {
            refuse_busy(context, agent, Busy::Background, Some(&text));
        }
        TurnOutcome::Failed(message) => {
            // Driver messages name the agent already. Spoken too, so the
            // error is heard with notifications off.
            error!("{message}");
            if context.notify_error() {
                send_notification_for("whisrs", &message, REPLY_NOTIFICATION_MS);
            }
            rt.say(&agent.name, rt.error_line(agent, &message));
        }
    }
}

/// The error for forgetting the session while a turn runs (`--reset`, or
/// `--new` while recording or sending).
fn reset_refusal(agent: &Agent) -> String {
    format!(
        "{STILL_WORKING} Stop it first: whisrs agent {} --stop",
        agent.name
    )
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
        return Err(reset_refusal(agent));
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

/// Stop the running turn and keep the session. With no whisrs turn
/// running, stop the session's own work, if it does any.
async fn stop_turn(context: &DaemonContext, agent: &Agent) -> Result<(), String> {
    let rt = &context.agents;
    let session = {
        let mut inner = rt.inner.lock().await;
        let Some(turn) = inner.slots.get_mut(&agent.name).and_then(|s| s.turn.take()) else {
            let idle_session = AgentRuntime::idle_session(&inner, agent);
            drop(inner);
            return stop_idle_session(context, agent, idle_session).await;
        };
        // Marked stopped first: a launch still running stops what it starts
        // itself, and the ids it reported already (a bg id whose session id
        // is still being looked up) are stopped here.
        let latest = turn.updates.stop();
        turn.task.abort();
        if let Some(slot) = inner.slots.get_mut(&agent.name) {
            slot.phase = AgentPhase::Idle;
        }
        let record = record_mut(&mut inner, agent);
        if let Some(latest) = latest {
            record.set_session(&latest);
        }
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

/// `--stop` with no whisrs turn running: stop the session when it works on
/// its own (what `whisrs agent` shows as working), else report that nothing
/// runs.
async fn stop_idle_session(
    context: &DaemonContext,
    agent: &Agent,
    session: Option<SessionRef>,
) -> Result<(), String> {
    let nothing = || format!("no turn is running for '{}'", agent.name);
    let Some(session) = session else {
        return Err(nothing());
    };
    let driver = context.agents.driver(agent.harness);
    match driver.session_phase(agent, &session).await {
        Ok(AgentPhase::Working | AgentPhase::NeedsYou) => {
            info!(
                "agent '{}': stopping the work its session does on its own",
                agent.name
            );
            driver
                .stop(agent, &session)
                .await
                .map_err(|e| format!("stopping the session failed: {e}"))?;
            // A needs-you that work set (a permission prompt with no turn
            // running) ends with it, so the row printed after the stop does
            // not say needs-you. A turn started meanwhile keeps its phase.
            let mut inner = context.agents.inner.lock().await;
            if let Some(slot) = inner.slots.get_mut(&agent.name) {
                if slot.turn.is_none() {
                    slot.phase = AgentPhase::Idle;
                }
            }
            Ok(())
        }
        Ok(AgentPhase::Idle) => Err(nothing()),
        Err(e) => Err(format!(
            "{}; cannot tell whether its session works: {e}",
            nothing()
        )),
    }
}

/// Speak and notify a busy refusal, and return the error for a CLI
/// request. `instruction` is the text that was not sent (transcribed or
/// typed), repeated in the notification so it can be sent again.
fn refuse_busy(
    context: &DaemonContext,
    agent: &Agent,
    busy: Busy,
    instruction: Option<&str>,
) -> String {
    let message = notify_busy(context, agent, busy, instruction);
    context.agents.say_notice(&agent.name, busy.notice());
    message
}

/// Log and notify a busy refusal (per `[general] notify`) without speaking
/// it, and return the error for a CLI request: [`refuse_busy`] minus the
/// notice, for a press that just stopped the reply or notice playing, so it
/// does not queue another one in its place.
fn notify_busy(
    context: &DaemonContext,
    agent: &Agent,
    busy: Busy,
    instruction: Option<&str>,
) -> String {
    match busy {
        Busy::Turn => info!("agent '{}': refused, a turn is running", agent.name),
        Busy::Background => info!(
            "agent '{}': refused, its session works on its own",
            agent.name
        ),
    }
    if context.notify_error() {
        send_notification_for(
            "whisrs",
            &busy_notification(&agent.name, busy, instruction),
            REPLY_NOTIFICATION_MS,
        );
    }
    busy.message(&agent.name)
}

// ---------------------------------------------------------------------------
// Voice turns: the recording sink
// ---------------------------------------------------------------------------

/// `whisrs agent <name>` or its hotkey; with `new_session` (`--new`, or its
/// `new_session_hotkey`) the transcript starts a fresh session.
///
/// - Recording for this agent: stop and transcribe (a press of either kind;
///   the press that started the recording decides about the session).
/// - An agent reply or notice playing: stop it first (barge-in), also while
///   this agent's turn runs, so a press always silences it.
/// - This agent's turn runs (`--new`: any turn, also one that waits for
///   you): refuse. With nothing stopped, the notice is spoken and notified;
///   a press that stopped what played only notifies, so it never queues
///   another notice in its place.
/// - Otherwise record a voice turn (a selection read-aloud is stopped for
///   it). Anything else: busy.
async fn voice_toggle(
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
    agent: Arc<Agent>,
    new_session: bool,
) -> Response {
    // Held from a barge-in stop until the recording started or the press
    // was refused, so the reply queue cannot start the next reply between
    // the stop and the recording.
    let mut barge_in = None;
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
            State::Speaking | State::Synthesizing => {
                let reason = format!("agent '{}'", agent.name);
                if crate::speak::stop_agent_playback(&mut ds, &context, &reason) {
                    barge_in = Some(context.agents.hold_replies());
                }
            }
            State::Idle => {}
        }
    }

    let rt = &context.agents;
    let busy = if new_session {
        rt.has_turn(&agent.name).await
    } else {
        rt.phase(&agent.name).await == AgentPhase::Working
    };
    if busy {
        let message = if barge_in.is_some() {
            info!(
                "agent '{}': the press stopped the playback; no notice is queued for the \
                 refusal",
                agent.name
            );
            notify_busy(&context, &agent, Busy::Turn, None)
        } else {
            refuse_busy(&context, &agent, Busy::Turn, None)
        };
        return error(&if new_session {
            reset_refusal(&agent)
        } else {
            message
        });
    }

    #[cfg(feature = "recording")]
    if let Some(refused) = refuse_while_recording(&context, DictationStart::Agent(&agent.name)) {
        return refused;
    }

    let response = start_recording(daemon_state, Arc::clone(&context), agent, new_session).await;
    drop(barge_in);
    response
}

/// Start recording a voice turn. Under one lock, so a reply that would start
/// speaking cannot slip in between stopping playback and the recording.
async fn start_recording(
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
    agent: Arc<Agent>,
    new_session: bool,
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
            new_session,
        });
        let _ = context.state_tx.send(ds.state_machine.state());
        audio_rx
    };

    let fresh = if new_session { " in a new session" } else { "" };
    info!("agent '{}': recording a voice turn{fresh}", agent.name);
    if context.notify_state() {
        send_notification(
            "whisrs",
            &format!("Speak to '{}'{fresh}... (press again to stop)", agent.name),
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
    let Some(claimed) = claimed else {
        info!("{label}: voice turn cancelled, recording discarded");
        return;
    };

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

    // Both failures are spoken as well as notified per [general] notify,
    // so they are heard with notifications off.
    let rt = &context.agents;
    let text = match transcript {
        Ok(text) => text,
        Err(e) => {
            let friendly = format_api_error(&e);
            error!("{label}: transcription failed: {e:#}");
            let message = format!("Agent '{}': transcription failed: {friendly}", agent.name);
            if context.notify_error() {
                send_notification("whisrs", &message);
            }
            rt.say(&agent.name, rt.error_line(&agent, &message));
            return;
        }
    };
    if text.trim().is_empty() {
        // Gated, a prompt echo or unintelligible: the helper notified.
        info!("{label}: nothing to send");
        rt.say(
            &agent.name,
            rt.line_for(&agent.name, DIDNT_CATCH.to_string()),
        );
        return;
    }
    if claimed.new_session {
        // A typed turn may have started while this one recorded: the
        // session is not forgotten under it.
        if reset(&context, &agent).await.is_err() {
            refuse_busy(&context, &agent, Busy::Turn, Some(&text));
            return;
        }
    }
    if let Err(busy) = start_turn(&context, &agent, text.clone()).await {
        refuse_busy(&context, &agent, busy, Some(&text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_agent(name: &str) -> Agent {
        Agent {
            name: name.into(),
            hotkey: None,
            new_session_hotkey: None,
            harness: Harness::Claude,
            dir: PathBuf::from("/proj"),
            permissions: whisrs::agent::Permissions::Auto,
            command: "claude".into(),
            args: Vec::new(),
            instruction: String::new(),
        }
    }

    /// A transcription backend that always fails, like an unreachable API.
    struct UnreachableBackend;

    #[async_trait::async_trait]
    impl whisrs::transcription::TranscriptionBackend for UnreachableBackend {
        async fn transcribe(
            &self,
            _audio: &[u8],
            _config: &whisrs::transcription::TranscriptionConfig,
        ) -> anyhow::Result<String> {
            anyhow::bail!("error sending request: connection refused")
        }

        fn sends_prompt(&self, _config: &whisrs::transcription::TranscriptionConfig) -> bool {
            false
        }

        fn stream_text(&self) -> whisrs::transcription::StreamText {
            whisrs::transcription::StreamText::Phrases
        }

        async fn transcribe_segments(
            &self,
            _audio: &[u8],
            _config: &whisrs::transcription::TranscriptionConfig,
        ) -> anyhow::Result<whisrs::transcription::Transcript> {
            unreachable!("voice turns do not request segments")
        }

        fn max_segment_chunk_secs(
            &self,
            _config: &whisrs::transcription::TranscriptionConfig,
        ) -> Option<u32> {
            None
        }
    }

    /// A TTS backend that never gets to synthesize in these tests.
    struct NoTts;

    #[async_trait::async_trait]
    impl whisrs::tts::TtsBackend for NoTts {
        async fn synthesize(
            &self,
            _text: &str,
        ) -> Result<Box<dyn whisrs::tts::TtsAudioStream>, whisrs::WhisrsError> {
            Err(whisrs::WhisrsError::Audio("no TTS in tests".into()))
        }
    }

    /// A context whose runtime drives `names` with the Claude driver and TTS
    /// on, its store in `dir`, nothing notified and no audio cue played.
    fn test_context(names: &[&str], dir: &Path) -> Arc<DaemonContext> {
        let mut rt = AgentRuntime::disabled();
        rt.speak = true;
        rt.agents = names.iter().map(|n| Arc::new(test_agent(n))).collect();
        rt.drivers.insert(
            Harness::Claude,
            Arc::new(ClaudeDriver::new(
                Path::new("whisrs"),
                Path::new("/nonexistent.sock"),
            )),
        );
        rt.store_path = dir.join("agents.json");
        Arc::new(DaemonContext {
            config: toml::from_str("[general]\naudio_feedback = false\n").unwrap(),
            window_tracker: Arc::new(whisrs::window::NoopTracker),
            transcription_backend: Arc::new(UnreachableBackend),
            notify: false,
            state_tx: tokio::sync::watch::channel(State::Idle).0,
            overlay_level_tx: None,
            overlay_enabled: false,
            #[cfg(feature = "recording")]
            recording: crate::recording::RecordingRuntime::new().0,
            agents: rt,
        })
    }

    /// Put a running turn of `generation` in the agent's slot.
    async fn install_turn(context: &DaemonContext, name: &str, generation: u64, text: &str) {
        let mut inner = context.agents.inner.lock().await;
        let slot = inner.slots.entry(name.to_string()).or_default();
        slot.phase = AgentPhase::Working;
        slot.turn = Some(RunningTurn {
            generation,
            started: Instant::now(),
            task: tokio::spawn(async {}),
            text: text.to_string(),
            hooks: mpsc::unbounded_channel().0,
            updates: TurnUpdates::channel().0,
        });
    }

    fn queued(rt: &AgentRuntime) -> Vec<String> {
        std::iter::from_fn(|| rt.next_utterance())
            .map(|u| u.text)
            .collect()
    }

    fn stop_hook(message: &str) -> serde_json::Value {
        serde_json::json!({
            "session_id": "sid-1",
            "hook_event_name": "Stop",
            "last_assistant_message": message,
        })
    }

    /// With no whisrs turn running: a permission prompt from the session's
    /// own work marks the agent needs-you and speaks the notice; its Stop is
    /// spoken and clears needs-you. The duplicate check compares raw text,
    /// is armed only by a turn that ended on a poll, and drops only that
    /// turn's late Stop: a real follow-up with the same spoken line is
    /// spoken.
    #[tokio::test]
    async fn hook_events_with_no_turn_running() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("t.jsonl");
        std::fs::write(
            &transcript,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#,
        )
        .unwrap();
        let context = test_context(&["jarvis"], dir.path());
        let rt = &context.agents;
        {
            let mut inner = rt.inner.lock().await;
            let mut record = AgentRecord::new("claude", Path::new("/proj"));
            record.session_id = Some("sid-1".into());
            record.bg_id = Some("bg1".into());
            record.transcript_path = Some(transcript);
            inner.store.agents.insert("jarvis".into(), record);
        }
        handle_agent_event(
            &context,
            serde_json::json!({
                "session_id": "sid-1",
                "hook_event_name": "Notification",
                "notification_type": "permission_prompt",
            }),
        )
        .await;
        assert_eq!(rt.phase("jarvis").await, AgentPhase::NeedsYou);
        assert_eq!(queued(rt), ["Waiting for your approval to use Bash."]);

        handle_agent_event(&context, stop_hook("Ran it. <spoken>Tests pass.</spoken>")).await;
        assert_eq!(rt.phase("jarvis").await, AgentPhase::Idle);
        handle_agent_event(&context, stop_hook("Again. <spoken>Tests pass.</spoken>")).await;
        assert_eq!(queued(rt), ["Tests pass.", "Tests pass."]);

        // Another session's events are not ours.
        handle_agent_event(
            &context,
            serde_json::json!({"session_id": "other", "hook_event_name": "Stop",
                               "last_assistant_message": "<spoken>Not ours.</spoken>"}),
        )
        .await;
        assert!(queued(rt).is_empty());

        // A turn that ended on a poll: its own late Stop is the duplicate.
        rt.inner
            .lock()
            .await
            .slots
            .get_mut("jarvis")
            .unwrap()
            .polled_reply = Some("Done.\n\n<spoken>All set.</spoken>".into());
        handle_agent_event(&context, stop_hook(" Done. <spoken>All set.</spoken>")).await;
        assert!(queued(rt).is_empty());
        handle_agent_event(&context, stop_hook("Done. <spoken>All set.</spoken>")).await;
        assert_eq!(queued(rt), ["All set."]);
    }

    /// Only a turn that ended on a poll arms the duplicate check; one that
    /// ended on its Stop clears it. A turn refused because the session
    /// works on its own speaks the background-task notice.
    #[tokio::test]
    async fn turn_ends_arm_the_duplicate_check_and_speak() {
        let dir = tempfile::tempdir().unwrap();
        let context = test_context(&["jarvis"], dir.path());
        let rt = &context.agents;
        let agent = Arc::clone(&rt.agents[0]);
        let polled = |inner: &Inner| inner.slots["jarvis"].polled_reply.clone();

        install_turn(&context, "jarvis", 7, "check it").await;
        let raw = "Checked. <spoken>All good.</spoken>";
        finish_turn(&context, &agent, 7, TurnOutcome::PolledReply(raw.into())).await;
        assert_eq!(polled(&*rt.inner.lock().await).as_deref(), Some(raw));
        assert_eq!(queued(rt), ["All good."]);

        install_turn(&context, "jarvis", 8, "again").await;
        finish_turn(&context, &agent, 8, TurnOutcome::Reply(raw.into())).await;
        assert_eq!(polled(&*rt.inner.lock().await), None);
        assert_eq!(queued(rt), ["All good."]);

        install_turn(&context, "jarvis", 9, "add a test").await;
        finish_turn(&context, &agent, 9, TurnOutcome::Busy).await;
        assert_eq!(queued(rt), [BACKGROUND_TASK]);
        assert_eq!(rt.phase("jarvis").await, AgentPhase::Idle);
        assert!(!rt.inner.lock().await.store.agents["jarvis"].turn_running);
    }

    /// The old process's Stop before a resumed turn's launch is spoken as a
    /// reply the session produced on its own.
    #[tokio::test]
    async fn a_stop_from_the_old_process_is_spoken_as_a_follow_up() {
        let dir = tempfile::tempdir().unwrap();
        let context = test_context(&["jarvis"], dir.path());
        let agent = Arc::clone(&context.agents.agents[0]);
        install_turn(&context, "jarvis", 3, "next").await;
        apply_update(
            &context,
            &agent,
            3,
            TurnUpdate::SessionReply("Finished. <spoken>The server is up.</spoken>".into()),
        )
        .await;
        assert_eq!(queued(&context.agents), ["The server is up."]);
        // The running turn is untouched.
        assert_eq!(context.agents.phase("jarvis").await, AgentPhase::Working);
    }

    /// A voice turn that had nothing to send, or whose transcription failed,
    /// is spoken, so it is heard with notifications off.
    #[tokio::test]
    async fn a_voice_turn_that_sends_nothing_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let context = test_context(&["jarvis"], dir.path());
        let agent = Arc::clone(&context.agents.agents[0]);
        let recording = || async {
            let ds = Arc::new(Mutex::new(DaemonState::new()));
            {
                let mut g = ds.lock().await;
                g.state_machine.transition(Action::Toggle).unwrap();
                g.agent_recording = Some(AgentRecordingContext {
                    name: "jarvis".into(),
                    new_session: false,
                });
            }
            ds
        };

        // No audio at all.
        let ds = recording().await;
        voice_turn_background(
            None,
            Arc::clone(&ds),
            Arc::clone(&context),
            Arc::clone(&agent),
        )
        .await;
        assert_eq!(queued(&context.agents), [DIDNT_CATCH]);
        assert_eq!(ds.lock().await.state_machine.state(), State::Idle);

        // Loud audio, and the backend cannot be reached.
        let ds = recording().await;
        let (tx, rx) = mpsc::unbounded_channel();
        let loud: Vec<i16> = (0..16_000)
            .map(|i| if i % 2 == 0 { 8_000 } else { -8_000 })
            .collect();
        tx.send(loud).unwrap();
        drop(tx);
        voice_turn_background(Some(rx), Arc::clone(&ds), Arc::clone(&context), agent).await;
        let lines = queued(&context.agents);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with("Transcription failed: Cannot reach API"),
            "{lines:?}"
        );
        assert_eq!(ds.lock().await.state_machine.state(), State::Idle);
        // No turn was started.
        assert!(context.agents.inner.lock().await.store.agents.is_empty());
    }

    /// A hold (a command press capturing its selection) keeps the reply
    /// queue from starting a reply: `speak_text` refuses it under the lock.
    /// Dropping the hold wakes the queue.
    #[tokio::test]
    async fn a_hold_keeps_agent_replies_from_starting() {
        let dir = tempfile::tempdir().unwrap();
        let context = test_context(&["jarvis"], dir.path());
        let ds = Arc::new(Mutex::new(DaemonState::new()));
        let hold = context.agents.hold_replies();
        assert!(replies_held(&context));
        let attempt = speak_text(
            Arc::clone(&ds),
            Arc::clone(&context),
            Box::new(NoTts),
            "a reply",
            SpeakOrigin::Agent,
        )
        .await;
        assert!(matches!(attempt, SpeakAttempt::NotIdle(State::Idle)));
        assert_eq!(ds.lock().await.state_machine.state(), State::Idle);
        assert!(ds.lock().await.tts_stop.is_none());
        let second = context.agents.hold_replies();
        drop(hold);
        assert!(replies_held(&context), "one hold is still live");
        drop(second);
        assert!(!replies_held(&context));
        tokio::time::timeout(
            Duration::from_secs(1),
            context.agents.utterance_ready.notified(),
        )
        .await
        .expect("a dropped hold wakes the reply queue");
    }

    /// Command mode and `[[llm_commands]]` release the dictation lock before
    /// their recording starts (command mode captures the selection), so both
    /// take a reply hold in the locked block that stopped agent playback:
    /// the queue cannot start the next reply in between and take the Idle
    /// state from them ("state transition failed").
    #[test]
    fn command_presses_hold_replies_until_they_record() {
        let code = include_str!("command_mode.rs");
        for signature in ["fn handle_command_mode(", "fn handle_llm_command("] {
            let start = code.find(signature).expect(signature);
            let body = &code[start..start + code[start..].find("\n}").unwrap()];
            let block = &body[..body.find("match current_state").expect(signature)];
            let lock = block.find("daemon_state.lock()").expect(signature);
            let stop = block.find("stop_agent_playback(").expect(signature);
            let hold = block.find("hold_replies()").expect(signature);
            assert!(lock < stop && stop < hold, "{signature}");
            // Held while the start runs: bound for the whole handler.
            assert!(block.contains("_hold)"), "{signature}");
        }
    }

    /// A reply of `jarvis`.
    fn utterance(text: &str) -> Utterance {
        Utterance {
            agent: "jarvis".into(),
            text: text.into(),
            notice: None,
        }
    }

    /// Put `ds` in a read-aloud of `origin` that plays, as `speak_text`
    /// does; returns its stop flag.
    async fn start_speaking(
        ds: &Mutex<DaemonState>,
        origin: SpeakOrigin,
    ) -> Arc<std::sync::atomic::AtomicBool> {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut g = ds.lock().await;
        g.state_machine.transition(Action::SpeakStart).unwrap();
        g.state_machine.transition(Action::SpeakPlaying).unwrap();
        g.tts_stop = Some(Arc::clone(&stop));
        g.tts_origin = Some(origin);
        stop
    }

    /// A recording start stops an agent reply being played, which plays
    /// again after the recording (first in the queue), and holds replies
    /// until the guard drops. A selection read-aloud is left alone.
    #[cfg(feature = "recording")]
    #[tokio::test]
    async fn a_recording_start_stops_an_agent_reply_and_plays_it_after() {
        let dir = tempfile::tempdir().unwrap();
        let context = test_context(&["jarvis"], dir.path());
        let rt = &context.agents;
        let ds = Arc::new(Mutex::new(DaemonState::new()));
        rt.daemon_state.set(Arc::clone(&ds)).ok().unwrap();

        let stop = start_speaking(&ds, SpeakOrigin::Agent).await;
        rt.set_playing(Some(utterance("first")));
        rt.say("jarvis", "second".into());
        let hold = hold_for_recording(&context);
        assert_eq!(ds.lock().await.state_machine.state(), State::Idle);
        assert!(stop.load(Ordering::Acquire), "playback was not stopped");
        assert!(replies_held(&context));
        drop(hold);
        assert!(!replies_held(&context));
        assert_eq!(queued(rt), ["first", "second"]);
        // The reply queue clears `playing` once it sees the playback end.
        rt.set_playing(None);

        // A selection read-aloud keeps playing, even while the queue is
        // about to speak (it then finds the daemon busy and waits).
        let stop = start_speaking(&ds, SpeakOrigin::Selection).await;
        rt.set_playing(Some(utterance("third")));
        let hold = hold_for_recording(&context);
        assert_eq!(ds.lock().await.state_machine.state(), State::Speaking);
        assert!(!stop.load(Ordering::Acquire));
        assert!(
            rt.is_speaking(),
            "only an agent reply that played is requeued"
        );
        drop(hold);
        assert!(queued(rt).is_empty());
    }

    /// The stop broadcasts Idle, and the reply queue, woken by it on another
    /// worker, can clear `playing` before the stopper puts the reply back.
    /// The reply is copied before the stop, so it still plays after the
    /// recording, once. Run here in that order: `playing` is cleared inside
    /// the stop, as the queue would between the stop and the requeue.
    #[cfg(feature = "recording")]
    #[test]
    fn a_stopped_reply_is_requeued_when_the_queue_clears_it_first() {
        let mut rt = AgentRuntime::disabled();
        rt.speak = true;
        rt.set_playing(Some(utterance("first")));
        rt.say("jarvis", "second".into());
        assert!(rt.requeue_if_stopped(|| {
            rt.set_playing(None);
            true
        }));
        assert_eq!(queued(&rt), ["first", "second"]);
        assert!(!rt.is_speaking());

        // Nothing was stopped: nothing is put back.
        rt.set_playing(Some(utterance("third")));
        assert!(!rt.requeue_if_stopped(|| false));
        assert!(queued(&rt).is_empty());
    }

    /// A recording start never waits on the dictation lock, which a
    /// dictation start holds while its mic opens: it holds replies at once
    /// and stops the agent reply playing once the lock is free.
    #[cfg(feature = "recording")]
    #[tokio::test]
    async fn a_recording_start_does_not_wait_on_the_dictation_lock() {
        let dir = tempfile::tempdir().unwrap();
        let context = test_context(&["jarvis"], dir.path());
        let rt = &context.agents;
        let ds = Arc::new(Mutex::new(DaemonState::new()));
        rt.daemon_state.set(Arc::clone(&ds)).ok().unwrap();
        let stop = start_speaking(&ds, SpeakOrigin::Agent).await;
        rt.set_playing(Some(utterance("first")));

        // A dictation start holds the lock.
        let dictation = ds.lock().await;
        let hold = hold_for_recording(&context);
        assert!(replies_held(&context));
        assert!(!stop.load(Ordering::Acquire), "stopped without the lock");
        assert!(!rt.has_utterances());
        drop(dictation);

        tokio::time::timeout(Duration::from_secs(1), async {
            while !rt.has_utterances() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the stop runs once the lock is free");
        assert!(stop.load(Ordering::Acquire), "playback was not stopped");
        assert_eq!(ds.lock().await.state_machine.state(), State::Idle);
        drop(hold);
        assert_eq!(queued(rt), ["first"]);

        // A start that is gone by the time the lock frees (nothing holds
        // replies) stops nothing.
        rt.set_playing(None);
        let stop = start_speaking(&ds, SpeakOrigin::Agent).await;
        rt.set_playing(Some(utterance("second")));
        let dictation = ds.lock().await;
        drop(hold_for_recording(&context));
        drop(dictation);
        // Let the spawned stop run (one runtime thread: it runs on the
        // first yield, and nothing in it waits once the lock is free).
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(ds.lock().await.state_machine.state(), State::Speaking);
        assert!(!stop.load(Ordering::Acquire));
        assert!(!rt.has_utterances());
    }

    /// A harness whose session waits on a permission prompt it hit on its
    /// own (no whisrs turn running), and stops when asked.
    struct PromptWaitingDriver;

    #[async_trait::async_trait]
    impl Driver for PromptWaitingDriver {
        async fn run_turn(
            &self,
            _agent: &Agent,
            _session: SessionRef,
            _request: TurnRequest,
            _io: TurnIo,
        ) -> TurnOutcome {
            unreachable!("no turn runs")
        }

        async fn rewatch(
            &self,
            _agent: &Agent,
            _session: SessionRef,
            _request: TurnRequest,
            _io: TurnIo,
        ) -> TurnOutcome {
            unreachable!("no turn runs")
        }

        async fn stop(&self, _agent: &Agent, _session: &SessionRef) -> Result<(), String> {
            Ok(())
        }

        fn hook_matches(&self, payload: &serde_json::Value, _session: &SessionRef) -> bool {
            payload["session_id"] == "sid-1"
        }

        async fn session_phase(
            &self,
            _agent: &Agent,
            _session: &SessionRef,
        ) -> Result<AgentPhase, String> {
            Ok(AgentPhase::NeedsYou)
        }

        async fn session_phases(
            &self,
            sessions: &[(Arc<Agent>, SessionRef)],
        ) -> Vec<Result<AgentPhase, String>> {
            sessions.iter().map(|_| Ok(AgentPhase::NeedsYou)).collect()
        }

        async fn idle_event(
            &self,
            _payload: &serde_json::Value,
            _session: &SessionRef,
        ) -> IdleEvent {
            IdleEvent::NeedsYou("Waiting for your approval to use Bash.".into())
        }
    }

    /// `--stop` with no whisrs turn running stops the session's own work,
    /// and the needs-you its permission prompt set goes with it: the row
    /// printed after the stop says idle.
    #[tokio::test]
    async fn stopping_a_sessions_own_work_clears_needs_you() {
        let dir = tempfile::tempdir().unwrap();
        let mut context = test_context(&["jarvis"], dir.path());
        Arc::get_mut(&mut context)
            .unwrap()
            .agents
            .drivers
            .insert(Harness::Claude, Arc::new(PromptWaitingDriver));
        {
            let mut inner = context.agents.inner.lock().await;
            let mut record = AgentRecord::new("claude", Path::new("/proj"));
            record.session_id = Some("sid-1".into());
            record.bg_id = Some("bg1".into());
            inner.store.agents.insert("jarvis".into(), record);
        }
        handle_agent_event(
            &context,
            serde_json::json!({
                "session_id": "sid-1",
                "hook_event_name": "Notification",
                "notification_type": "permission_prompt",
            }),
        )
        .await;
        assert_eq!(context.agents.phase("jarvis").await, AgentPhase::NeedsYou);

        let ds = Arc::new(Mutex::new(DaemonState::new()));
        let response = handle_agent(
            ds,
            Arc::clone(&context),
            AgentAction::Stop,
            Some("jarvis".into()),
            None,
            false,
        )
        .await;
        match response {
            Response::Agents { agents } => assert_eq!(agents[0].phase, AgentPhase::Idle),
            other => panic!("expected the agent's row, got {other:?}"),
        }
    }

    #[test]
    fn the_still_working_refusal_is_not_notified_twice() {
        assert!(error_already_notified(STILL_WORKING));
        assert!(error_already_notified(&format!(
            "{STILL_WORKING} Stop it first: whisrs agent jarvis --stop"
        )));
        // The CLI errors of both refusals are what was notified already.
        assert!(error_already_notified(&Busy::Turn.message("jarvis")));
        assert!(error_already_notified(&Busy::Background.message("jarvis")));
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
        let at = |received: Instant, payload| HookEvent { received, payload };
        let mut events: VecDeque<_> = [
            at(t0, ev("ours", 1)),
            at(t0, ev("other", 2)),
            at(start, ev("ours", 3)),
            at(start + Duration::from_secs(1), ev("ours", 4)),
        ]
        .into_iter()
        .collect();
        let (fresh, stale) = take_session_events(&mut events, |p| p["session_id"] == "ours", start);
        let payloads: Vec<_> = fresh.iter().map(|e| e.payload.clone()).collect();
        assert_eq!(payloads, vec![ev("ours", 3), ev("ours", 4)]);
        // The arrival time travels with the event, for the driver.
        assert_eq!(fresh[1].received, start + Duration::from_secs(1));
        assert_eq!(stale, 1);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload, ev("other", 2));
    }

    /// Replies wait while a recording captures, even with the daemon Idle,
    /// and while dictation, a command or a read-aloud runs.
    #[test]
    fn replies_wait_for_idle_and_no_recording() {
        assert!(may_speak(State::Idle, false));
        assert!(!may_speak(State::Idle, true));
        for state in [
            State::Recording,
            State::Transcribing,
            State::Synthesizing,
            State::Speaking,
        ] {
            assert!(!may_speak(state, false), "{state:?}");
            assert!(!may_speak(state, true), "{state:?}");
        }
    }

    /// One busy notice of a kind per agent at a time: not queued again while
    /// it is queued or playing. With several agents the spoken line names
    /// the agent, and the check still holds: it compares the notice, not
    /// the line. Other utterances are never deduplicated.
    #[test]
    fn a_busy_notice_is_not_queued_twice() {
        let mut rt = AgentRuntime::disabled();
        rt.speak = true;
        rt.agents = vec![Arc::new(test_agent("a")), Arc::new(test_agent("b"))];
        rt.say_notice("a", STILL_WORKING);
        rt.say_notice("a", STILL_WORKING);
        rt.say_notice("b", STILL_WORKING);
        rt.say_notice("a", BACKGROUND_TASK);
        rt.say_notice("a", BACKGROUND_TASK);
        rt.say("a", "Done.".into());
        rt.say("a", "Done.".into());
        let queued: Vec<_> = std::iter::from_fn(|| rt.next_utterance())
            .map(|u| u.text)
            .collect();
        assert_eq!(
            queued,
            [
                format!("a: {STILL_WORKING}"),
                format!("b: {STILL_WORKING}"),
                format!("a: {BACKGROUND_TASK}"),
                "Done.".to_string(),
                "Done.".to_string()
            ]
        );
        // Playing counts too, until its playback ends.
        rt.set_playing(Some(Utterance {
            agent: "a".into(),
            text: format!("a: {STILL_WORKING}"),
            notice: Some(STILL_WORKING),
        }));
        rt.say_notice("a", STILL_WORKING);
        assert!(!rt.has_utterances());
        rt.set_playing(None);
        rt.say_notice("a", STILL_WORKING);
        assert!(rt.has_utterances());
        // `whisrs cancel` while a reply plays drops the rest.
        rt.say("b", "Another reply.".into());
        rt.drop_queued();
        assert!(!rt.has_utterances());
        // One agent: the notice is spoken as is.
        rt.agents.truncate(1);
        rt.say_notice("a", BACKGROUND_TASK);
        assert_eq!(rt.next_utterance().unwrap().text, BACKGROUND_TASK);
    }

    /// A refusal after the instruction was transcribed or typed repeats it,
    /// so it can be sent again; the background-task one says how to end
    /// that task.
    #[test]
    fn a_refused_instruction_is_in_the_notification() {
        assert_eq!(
            busy_notification("jarvis", Busy::Turn, None),
            "jarvis: Still working on the last one."
        );
        assert_eq!(
            busy_notification("jarvis", Busy::Turn, Some(" add a test ")),
            "jarvis: Still working on the last one. Not sent: \"add a test\"."
        );
        assert_eq!(
            busy_notification("jarvis", Busy::Background, Some("add a test")),
            "jarvis: It's still running a background task. Not sent: \"add a test\". \
             `whisrs agent jarvis --stop` ends that task, then send it again."
        );
        let long = "word ".repeat(400);
        let body = busy_notification("jarvis", Busy::Turn, Some(&long));
        assert!(body.len() < REFUSED_INSTRUCTION_MAX + 100, "{}", body.len());
        assert_eq!(Busy::Turn.notice(), STILL_WORKING);
        assert_eq!(Busy::Background.notice(), BACKGROUND_TASK);
        assert!(Busy::Background
            .message("jarvis")
            .contains("whisrs agent jarvis --stop"));
    }

    #[test]
    fn replies_are_compared_up_to_whitespace() {
        assert!(same_reply(
            "Done.\n\n<spoken>All set.</spoken>",
            " Done. <spoken>All set.</spoken>\n"
        ));
        assert!(!same_reply("<spoken>Done.</spoken>", "Done."));
        assert!(!same_reply(
            "Fixed the test. <spoken>Done.</spoken>",
            "Ran it. <spoken>Done.</spoken>"
        ));
    }

    #[test]
    fn errors_are_spoken_short_and_name_the_agent_when_there_are_several() {
        let mut rt = AgentRuntime::disabled();
        let agent = test_agent;
        let message = "Agent 'jarvis': That turn was interrupted, say it again.";
        rt.agents = vec![Arc::new(agent("jarvis"))];
        assert_eq!(
            rt.error_line(&agent("jarvis"), message),
            "That turn was interrupted, say it again."
        );
        rt.agents.push(Arc::new(agent("friday")));
        assert_eq!(
            rt.error_line(&agent("jarvis"), message),
            "jarvis: That turn was interrupted, say it again."
        );
    }

    /// A press while an agent reply or notice plays during this agent's
    /// running turn stops the playback, and is refused without queuing
    /// another notice in its place. With nothing playing, the refusal is
    /// spoken as before.
    #[tokio::test]
    async fn a_press_during_a_running_turn_stops_playback_without_a_new_notice() {
        let dir = tempfile::tempdir().unwrap();
        let context = test_context(&["jarvis"], dir.path());
        let rt = &context.agents;
        let agent = Arc::clone(&rt.agents[0]);
        install_turn(&context, "jarvis", 1, "the running one").await;
        let ds = Arc::new(Mutex::new(DaemonState::new()));

        // The "Still working" notice of an earlier press plays.
        let stop = start_speaking(&ds, SpeakOrigin::Agent).await;
        rt.set_playing(Some(Utterance {
            agent: "jarvis".into(),
            text: STILL_WORKING.into(),
            notice: Some(STILL_WORKING),
        }));
        let response = voice_toggle(
            Arc::clone(&ds),
            Arc::clone(&context),
            Arc::clone(&agent),
            false,
        )
        .await;
        assert!(
            matches!(&response, Response::Error { message } if message == STILL_WORKING),
            "{response:?}"
        );
        assert!(stop.load(Ordering::Acquire), "the notice was not stopped");
        assert_eq!(ds.lock().await.state_machine.state(), State::Idle);
        // The queue clears `playing` once it sees the playback end.
        rt.set_playing(None);
        assert!(queued(rt).is_empty(), "a notice was queued");
        assert!(
            !replies_held(&context),
            "the barge-in hold outlived the press"
        );

        // A follow-up reply plays: the same.
        let stop = start_speaking(&ds, SpeakOrigin::Agent).await;
        rt.set_playing(Some(utterance("A reply it produced on its own.")));
        voice_toggle(
            Arc::clone(&ds),
            Arc::clone(&context),
            Arc::clone(&agent),
            false,
        )
        .await;
        assert!(stop.load(Ordering::Acquire));
        rt.set_playing(None);
        assert!(queued(rt).is_empty());

        // Nothing plays: the refusal is spoken.
        voice_toggle(Arc::clone(&ds), Arc::clone(&context), agent, false).await;
        assert_eq!(queued(rt), [STILL_WORKING]);
    }

    /// A selection read-aloud is not an agent's playback: a press while the
    /// turn runs leaves it playing and is refused as before.
    #[tokio::test]
    async fn a_press_during_a_running_turn_leaves_a_selection_read_aloud() {
        let dir = tempfile::tempdir().unwrap();
        let context = test_context(&["jarvis"], dir.path());
        let agent = Arc::clone(&context.agents.agents[0]);
        install_turn(&context, "jarvis", 1, "the running one").await;
        let ds = Arc::new(Mutex::new(DaemonState::new()));
        let stop = start_speaking(&ds, SpeakOrigin::Selection).await;
        voice_toggle(Arc::clone(&ds), Arc::clone(&context), agent, false).await;
        assert!(!stop.load(Ordering::Acquire));
        assert_eq!(ds.lock().await.state_machine.state(), State::Speaking);
        assert_eq!(queued(&context.agents), [STILL_WORKING]);
    }

    /// `--new` is refused while any turn of the agent runs, also one that
    /// waits for you (a plain press would answer that one), and says how to
    /// go on.
    #[tokio::test]
    async fn a_fresh_session_press_is_refused_while_a_turn_runs() {
        let dir = tempfile::tempdir().unwrap();
        let context = test_context(&["jarvis"], dir.path());
        let agent = Arc::clone(&context.agents.agents[0]);
        install_turn(&context, "jarvis", 1, "the running one").await;
        context
            .agents
            .inner
            .lock()
            .await
            .slots
            .get_mut("jarvis")
            .unwrap()
            .phase = AgentPhase::NeedsYou;
        let ds = Arc::new(Mutex::new(DaemonState::new()));
        let response = voice_toggle(Arc::clone(&ds), Arc::clone(&context), agent, true).await;
        assert!(
            matches!(&response, Response::Error { message }
                if message == "Still working on the last one. Stop it first: whisrs agent jarvis --stop"),
            "{response:?}"
        );
        assert_eq!(ds.lock().await.state_machine.state(), State::Idle);
        assert_eq!(queued(&context.agents), [STILL_WORKING]);
    }

    /// A transcription backend that always hears the same words.
    struct FixedBackend;

    #[async_trait::async_trait]
    impl whisrs::transcription::TranscriptionBackend for FixedBackend {
        async fn transcribe(
            &self,
            _audio: &[u8],
            _config: &whisrs::transcription::TranscriptionConfig,
        ) -> anyhow::Result<String> {
            Ok("run the tests".into())
        }

        fn sends_prompt(&self, _config: &whisrs::transcription::TranscriptionConfig) -> bool {
            false
        }

        fn stream_text(&self) -> whisrs::transcription::StreamText {
            whisrs::transcription::StreamText::Phrases
        }

        async fn transcribe_segments(
            &self,
            _audio: &[u8],
            _config: &whisrs::transcription::TranscriptionConfig,
        ) -> anyhow::Result<whisrs::transcription::Transcript> {
            unreachable!("voice turns do not request segments")
        }

        fn max_segment_chunk_secs(
            &self,
            _config: &whisrs::transcription::TranscriptionConfig,
        ) -> Option<u32> {
            None
        }
    }

    /// A harness that records the session each turn starts in and replies
    /// at once.
    #[derive(Default)]
    struct SessionLoggingDriver {
        sessions: std::sync::Mutex<Vec<SessionRef>>,
    }

    #[async_trait::async_trait]
    impl Driver for SessionLoggingDriver {
        async fn run_turn(
            &self,
            _agent: &Agent,
            session: SessionRef,
            _request: TurnRequest,
            _io: TurnIo,
        ) -> TurnOutcome {
            self.sessions.lock().unwrap().push(session);
            TurnOutcome::Reply("<spoken>Done.</spoken>".into())
        }

        async fn rewatch(
            &self,
            _agent: &Agent,
            _session: SessionRef,
            _request: TurnRequest,
            _io: TurnIo,
        ) -> TurnOutcome {
            unreachable!("no turn is watched again")
        }

        async fn stop(&self, _agent: &Agent, _session: &SessionRef) -> Result<(), String> {
            Ok(())
        }

        fn hook_matches(&self, _payload: &serde_json::Value, _session: &SessionRef) -> bool {
            false
        }

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
            sessions.iter().map(|_| Ok(AgentPhase::Idle)).collect()
        }

        async fn idle_event(
            &self,
            _payload: &serde_json::Value,
            _session: &SessionRef,
        ) -> IdleEvent {
            IdleEvent::Ignored
        }
    }

    /// A voice turn whose press asked for a fresh session forgets the stored
    /// one when its transcript is ready and starts the turn in a new
    /// session; a plain one continues the stored session. A turn that
    /// started while the fresh one recorded keeps the session, and the
    /// instruction is refused.
    #[tokio::test]
    async fn a_fresh_session_voice_turn_starts_a_new_session() {
        let dir = tempfile::tempdir().unwrap();
        let driver = Arc::new(SessionLoggingDriver::default());
        let mut context = test_context(&["jarvis"], dir.path());
        {
            let ctx = Arc::get_mut(&mut context).unwrap();
            ctx.transcription_backend = Arc::new(FixedBackend);
            ctx.agents
                .drivers
                .insert(Harness::Claude, Arc::clone(&driver) as Arc<dyn Driver>);
        }
        let agent = Arc::clone(&context.agents.agents[0]);
        let stored = SessionRef {
            session_id: Some("sid-1".into()),
            bg_id: Some("bg1".into()),
            transcript_path: None,
        };
        let store_session = |context: Arc<DaemonContext>| {
            let stored = stored.clone();
            async move {
                let mut inner = context.agents.inner.lock().await;
                let mut record = AgentRecord::new("claude", Path::new("/proj"));
                record.set_session(&stored);
                inner.store.agents.insert("jarvis".into(), record);
            }
        };
        let voice_turn = |new_session: bool| {
            let context = Arc::clone(&context);
            let agent = Arc::clone(&agent);
            async move {
                let ds = Arc::new(Mutex::new(DaemonState::new()));
                {
                    let mut g = ds.lock().await;
                    g.state_machine.transition(Action::Toggle).unwrap();
                    g.agent_recording = Some(AgentRecordingContext {
                        name: "jarvis".into(),
                        new_session,
                    });
                }
                let (tx, rx) = mpsc::unbounded_channel();
                let loud: Vec<i16> = (0..16_000)
                    .map(|i| if i % 2 == 0 { 8_000 } else { -8_000 })
                    .collect();
                tx.send(loud).unwrap();
                drop(tx);
                voice_turn_background(Some(rx), ds, context, agent).await;
            }
        };
        let turns = |n: usize| {
            let driver = Arc::clone(&driver);
            async move {
                tokio::time::timeout(Duration::from_secs(5), async {
                    while driver.sessions.lock().unwrap().len() < n {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .expect("the turn did not start");
                driver.sessions.lock().unwrap()[n - 1].clone()
            }
        };

        // Wait until the turn ended and its reply was queued.
        let replied = || async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !context.agents.has_utterances() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("the turn did not reply");
            assert_eq!(queued(&context.agents), ["Done."]);
            assert!(!context.agents.has_turn("jarvis").await);
        };

        store_session(Arc::clone(&context)).await;
        voice_turn(false).await;
        assert_eq!(turns(1).await, stored, "a plain voice turn continues");
        replied().await;

        store_session(Arc::clone(&context)).await;
        voice_turn(true).await;
        assert_eq!(turns(2).await, SessionRef::default(), "not a fresh session");
        replied().await;

        // A turn runs by the time the fresh one is transcribed.
        store_session(Arc::clone(&context)).await;
        install_turn(&context, "jarvis", 99, "typed meanwhile").await;
        voice_turn(true).await;
        assert_eq!(queued(&context.agents), [STILL_WORKING]);
        let inner = context.agents.inner.lock().await;
        assert_eq!(inner.store.agents["jarvis"].session(), stored);
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
