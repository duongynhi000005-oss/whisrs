//! Desktop overlays: the pill shown while recording or transcribing, and
//! the agent overlay.

use std::io::{ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;

use tokio::sync::watch;
use tracing::{info, warn};

use crate::agent::display::AgentOverlayEntry;
use crate::State;

use super::agent_host::{AgentFrames, ShownFlag};
use super::agent_wayland::run_agent_overlay;
use super::agent_x11::run_agent_x11_overlay;
use super::gnome::{is_gnome_desktop, run_gnome_broadcaster, GnomeAgents, GnomePill};
use super::wayland::run_overlay;
use super::x11::run_x11_overlay;
use super::{AgentOverlayShown, AgentsFeed, OverlayFeeds, PillFeed};

#[derive(Debug, thiserror::Error)]
pub(super) enum OverlayError {
    #[error("Wayland connection error: {0}")]
    Connect(#[from] wayland_client::ConnectError),
    #[error("Wayland globals error: {0}")]
    Globals(#[from] wayland_client::globals::GlobalError),
    #[error("smithay bind error: {0}")]
    Bind(#[from] wayland_client::globals::BindError),
    #[error("smithay shm create error: {0}")]
    Shm(#[from] smithay_client_toolkit::shm::CreatePoolError),
    #[error("Wayland dispatch error: {0}")]
    Dispatch(#[from] wayland_client::DispatchError),
    #[error("Wayland connection error: {0}")]
    Wayland(#[from] wayland_client::backend::WaylandError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("X11 display connect error: {0}")]
    X11Connect(#[from] x11rb::errors::ConnectError),
    #[error("X11 protocol/IO error: {0}")]
    X11Connection(#[from] x11rb::errors::ConnectionError),
    #[error("X11 reply error: {0}")]
    X11Reply(#[from] x11rb::errors::ReplyError),
    #[error("X11 reply/id error: {0}")]
    X11ReplyOrId(#[from] x11rb::errors::ReplyOrIdError),
    #[error("X11 ARGB visual not available")]
    X11Visual,
    #[error("D-Bus error: {0}")]
    DBus(#[from] zbus::Error),
    #[error("D-Bus signal error: {0}")]
    DBusSignal(#[from] zbus::fdo::Error),
    #[error("tiny-skia pixmap allocation failed for {0}x{1}")]
    Pixmap(u32, u32),
}

/// Spawn the overlays `feeds` asks for, each on its own thread and display
/// connection, and say whether the agent overlay is on screen.
///
/// Native compositor event loops run on dedicated OS threads because they
/// are blocking client loops. Small Tokio tasks forward daemon state
/// changes into them. On GNOME one D-Bus broadcaster carries both. The
/// agent overlay is not drawn natively on GNOME Wayland (no layer-shell),
/// so it is never shown there. Must run inside a Tokio runtime.
pub fn spawn_overlays(feeds: OverlayFeeds) -> AgentOverlayShown {
    let (shown_tx, shown_rx) = watch::channel(false);
    let OverlayFeeds { pill, agents } = feeds;
    let backend = OverlayBackend::detect();
    let gnome = is_gnome_desktop();

    if gnome && (pill.is_some() || agents.is_some()) {
        let gnome_pill = pill.as_ref().map(|p| GnomePill {
            state_rx: p.state_rx.clone(),
            level_rx: p.level_rx.clone(),
            theme: p.config.theme.clone(),
            position: p.config.position(),
        });
        let gnome_agents = agents.as_ref().map(|a| GnomeAgents {
            entries_rx: a.entries_rx.clone(),
            style: a.config.style(),
            corner: a.config.corner(),
        });
        tokio::spawn(async move {
            if let Err(e) = run_gnome_broadcaster(gnome_pill, gnome_agents).await {
                warn!("GNOME overlay D-Bus broadcaster unavailable: {e:#}");
            }
        });
    }

    if let Some(pill) = pill {
        spawn_pill(pill, backend);
    }
    if let Some(agents) = agents {
        if gnome && backend == OverlayBackend::Wayland {
            info!(
                "agent overlay: GNOME on Wayland has no layer-shell, so it is only broadcast \
                 (AgentsChanged); agent notifications stay on"
            );
        } else {
            spawn_agents(agents, backend, ShownFlag::new(shown_tx));
        }
    }
    AgentOverlayShown(shown_rx)
}

/// The pill: its thread, and the task forwarding state and level into it.
fn spawn_pill(feed: PillFeed, backend: OverlayBackend) {
    let PillFeed {
        mut state_rx,
        mut level_rx,
        config,
    } = feed;
    let (tx, rx) = mpsc::channel::<State>();
    let (level_tx, level_rx_thread) = mpsc::channel::<f32>();

    info!("overlay backend selected: {backend:?}");
    let overlay_config = config;
    std::thread::Builder::new()
        .name("whisrs-overlay".to_string())
        .spawn(move || {
            let result = match backend {
                OverlayBackend::Wayland => run_overlay(rx, level_rx_thread, overlay_config),
                OverlayBackend::X11 => run_x11_overlay(rx, level_rx_thread, overlay_config),
                OverlayBackend::Unavailable => {
                    warn!("overlay unavailable: no Wayland or X11 display in environment");
                    return;
                }
            };
            if let Err(e) = result {
                warn!("overlay unavailable: {e:#}");
            }
        })
        .map_err(|e| warn!("failed to spawn overlay thread: {e}"))
        .ok();

    tokio::spawn(async move {
        let _ = tx.send(*state_rx.borrow());
        let _ = level_tx.send(*level_rx.borrow());
        loop {
            tokio::select! {
                changed = state_rx.changed() => {
                    if changed.is_err() { break; }
                    if tx.send(*state_rx.borrow()).is_err() { break; }
                }
                changed = level_rx.changed() => {
                    if changed.is_err() { break; }
                    let _ = level_tx.send(*level_rx.borrow());
                }
            }
        }
    });
}

/// The agent overlay: its thread, and the task waking it per board update.
/// The thread reads the board and the level itself (`AgentFrames`).
fn spawn_agents(feed: AgentsFeed, backend: OverlayBackend, shown: ShownFlag) {
    let AgentsFeed {
        entries_rx,
        level_rx,
        config,
    } = feed;
    let (style, corner) = (config.style(), config.corner());
    info!(
        "agent overlay backend selected: {backend:?} ({}, {})",
        style.as_str(),
        corner.as_str()
    );
    let forward_rx = entries_rx.clone();
    let frames = move || AgentFrames::new(entries_rx, level_rx, style, corner);
    match backend {
        OverlayBackend::Wayland => {
            let (wake_rx, wake_tx) = match UnixStream::pair() {
                Ok(pair) => pair,
                Err(e) => {
                    warn!("agent overlay unavailable: no wake socket: {e}");
                    return;
                }
            };
            if let Err(e) = wake_tx.set_nonblocking(true) {
                warn!("agent overlay unavailable: no wake socket: {e}");
                return;
            }
            let spawned =
                spawn_agent_thread(move || run_agent_overlay(frames()?, wake_rx, corner, shown));
            if spawned {
                forward_board(forward_rx, move || wake_socket(&wake_tx));
            }
        }
        OverlayBackend::X11 => {
            let (wake_tx, wake_rx) = mpsc::channel::<()>();
            let spawned = spawn_agent_thread(move || {
                run_agent_x11_overlay(frames()?, wake_rx, corner, shown)
            });
            if spawned {
                forward_board(forward_rx, move || wake_tx.send(()).is_ok());
            }
        }
        OverlayBackend::Unavailable => {
            warn!("agent overlay unavailable: no Wayland or X11 display in environment");
        }
    }
}

/// Run an agent overlay host on its own thread. `false` if it could not
/// start.
fn spawn_agent_thread(run: impl FnOnce() -> Result<(), OverlayError> + Send + 'static) -> bool {
    std::thread::Builder::new()
        .name("whisrs-agent-overlay".to_string())
        .spawn(move || {
            if let Err(e) = run() {
                warn!("agent overlay unavailable: {e:#}");
            }
        })
        .map_err(|e| warn!("failed to spawn agent overlay thread: {e}"))
        .is_ok()
}

/// Call `wake` on every board change, until the board or (`wake` says
/// `false`) the host is gone.
fn forward_board(
    mut entries_rx: watch::Receiver<Vec<AgentOverlayEntry>>,
    mut wake: impl FnMut() -> bool + Send + 'static,
) {
    tokio::spawn(async move {
        while entries_rx.changed().await.is_ok() {
            if !wake() {
                break;
            }
        }
    });
}

/// Write one wake byte. A full socket already holds a wake; a closed one
/// means the host ended.
fn wake_socket(mut socket: &UnixStream) -> bool {
    match socket.write_all(&[1]) {
        Ok(()) => true,
        Err(e) => e.kind() == ErrorKind::WouldBlock,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverlayBackend {
    Wayland,
    X11,
    Unavailable,
}

impl OverlayBackend {
    fn detect() -> Self {
        let session_is_wayland = matches_env("XDG_SESSION_TYPE", "wayland");
        let session_is_x11 = matches_env("XDG_SESSION_TYPE", "x11");
        if (env_var_is_set("WAYLAND_DISPLAY") || session_is_wayland) && !session_is_x11 {
            Self::Wayland
        } else if env_var_is_set("DISPLAY") {
            Self::X11
        } else {
            Self::Unavailable
        }
    }
}

fn env_var_is_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn matches_env(name: &str, expected: &str) -> bool {
    std::env::var(name).is_ok_and(|value| value.eq_ignore_ascii_case(expected))
}
