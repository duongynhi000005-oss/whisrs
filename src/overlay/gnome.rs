//! GNOME overlay D-Bus broadcaster.
//!
//! One connection owns `org.whisrs.Overlay` and carries both overlays: the
//! pill's signals (`StateChanged`, `LevelChanged`, `ThemeChanged`,
//! `PositionChanged`) and the agent overlay's `AgentsChanged`, a JSON
//! string:
//!
//! ```json
//! {"style": "orb", "corner": "bottom-right",
//!  "agents": [{"name": "jarvis", "state": "working",
//!              "activity": "Running a command", "elapsed_s": 12}]}
//! ```
//!
//! sent on every change and every second while an agent is shown (the
//! timer). The GNOME extension does not draw the agent overlay yet, so on
//! GNOME Wayland (no layer-shell) the agent toasts stay on.

use std::time::{Duration, Instant};

use tokio::sync::watch;
use tracing::{info, warn};

use crate::agent::display::AgentOverlayEntry;
use crate::{AgentOverlayCorner, AgentOverlayStyle, OverlayPosition, State};

use super::service::OverlayError;

/// What the pill's signals carry.
pub(super) struct GnomePill {
    pub(super) state_rx: watch::Receiver<State>,
    pub(super) level_rx: watch::Receiver<f32>,
    pub(super) theme: String,
    pub(super) position: OverlayPosition,
}

/// What `AgentsChanged` carries.
pub(super) struct GnomeAgents {
    pub(super) entries_rx: watch::Receiver<Vec<AgentOverlayEntry>>,
    pub(super) style: AgentOverlayStyle,
    pub(super) corner: AgentOverlayCorner,
}

pub(super) fn is_gnome_desktop() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .map(|value| {
            value
                .split(':')
                .any(|part| part.eq_ignore_ascii_case("GNOME"))
        })
        .unwrap_or(false)
}

/// Own the bus name and broadcast for the overlays given, each until its
/// channel closes or a signal fails (logged). Errs only when the bus name
/// cannot be had.
pub(super) async fn run_gnome_broadcaster(
    pill: Option<GnomePill>,
    agents: Option<GnomeAgents>,
) -> Result<(), OverlayError> {
    let conn = zbus::connection::Builder::session()?
        .serve_at("/org/whisrs/Overlay", GnomeOverlayBus)?
        .name("org.whisrs.Overlay")?
        .build()
        .await?;

    info!("GNOME overlay D-Bus broadcaster started");
    let pill = async {
        if let Some(pill) = pill {
            if let Err(e) = run_gnome_pill(&conn, pill).await {
                warn!("GNOME overlay D-Bus broadcaster unavailable: {e:#}");
            }
        }
    };
    let agents = async {
        if let Some(agents) = agents {
            if let Err(e) = run_gnome_agents(&conn, agents).await {
                warn!("GNOME agent overlay broadcast stopped: {e:#}");
            }
        }
    };
    tokio::join!(pill, agents);
    Ok(())
}

async fn run_gnome_pill(conn: &zbus::Connection, pill: GnomePill) -> Result<(), OverlayError> {
    let GnomePill {
        mut state_rx,
        level_rx,
        theme,
        position,
    } = pill;
    // Custom themes don't sync over D-Bus for v1 — the GNOME extension only
    // knows the named themes it ships. Fall back to "ember" so the bar
    // colors remain sensible.
    let advertised_theme = match theme.as_str() {
        "carbon" | "cyan" | "ember" => theme.clone(),
        _ => "ember".to_string(),
    };

    emit_gnome_theme(conn, &advertised_theme).await?;
    emit_gnome_position(conn, position).await?;
    let initial_state = *state_rx.borrow();
    emit_gnome_state(conn, initial_state).await?;
    let initial_level = *level_rx.borrow();
    emit_gnome_level(conn, initial_level).await?;

    // Emit level at a steady ~30 Hz so the GNOME extension always has fresh
    // data, regardless of how the watch channel coalesces updates.
    let mut level_interval = tokio::time::interval(Duration::from_millis(33));
    level_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            changed = state_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let state = *state_rx.borrow();
                // GNOME re-enables the extension on unlock with its defaults,
                // and it may start after us, so a one-shot theme/position at
                // startup is lost. Re-send both ahead of every state change;
                // the pill is hidden while idle, so it is placed before it shows.
                emit_gnome_theme(conn, &advertised_theme).await?;
                emit_gnome_position(conn, position).await?;
                emit_gnome_state(conn, state).await?;
            }
            _ = level_interval.tick() => {
                let level = *level_rx.borrow();
                emit_gnome_level(conn, level).await?;
            }
        }
    }

    Ok(())
}

/// Send `AgentsChanged` now, on every board change, and every second
/// while a row is shown, so the timers count. A tick counts from the last
/// send.
async fn run_gnome_agents(
    conn: &zbus::Connection,
    agents: GnomeAgents,
) -> Result<(), OverlayError> {
    let GnomeAgents {
        mut entries_rx,
        style,
        corner,
    } = agents;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let emit = |entries: &[AgentOverlayEntry]| {
        emit_gnome_agents(conn, agents_json(entries, style, corner, Instant::now()))
    };
    let entries = entries_rx.borrow_and_update().clone();
    emit(&entries).await?;
    tick.reset();
    loop {
        let shown = !entries_rx.borrow().is_empty();
        tokio::select! {
            changed = entries_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let entries = entries_rx.borrow_and_update().clone();
                emit(&entries).await?;
                tick.reset();
            }
            _ = tick.tick(), if shown => {
                let entries = entries_rx.borrow().clone();
                emit(&entries).await?;
            }
        }
    }
    Ok(())
}

/// `AgentsChanged`'s JSON (see the module docs). `elapsed_s` is the
/// card's timer, from the entry's `since` to `now`.
fn agents_json(
    entries: &[AgentOverlayEntry],
    style: AgentOverlayStyle,
    corner: AgentOverlayCorner,
    now: Instant,
) -> String {
    let agents: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "name": e.name,
                "state": e.visual.as_str(),
                "activity": e.activity,
                "elapsed_s": now.saturating_duration_since(e.since).as_secs(),
            })
        })
        .collect();
    serde_json::json!({
        "style": style.as_str(),
        "corner": corner.as_str(),
        "agents": agents,
    })
    .to_string()
}

async fn emit_gnome_agents(conn: &zbus::Connection, json: String) -> zbus::Result<()> {
    conn.emit_signal(
        None::<&str>,
        "/org/whisrs/Overlay",
        "org.whisrs.Overlay",
        "AgentsChanged",
        &json,
    )
    .await
}

async fn emit_gnome_state(conn: &zbus::Connection, state: State) -> zbus::Result<()> {
    conn.emit_signal(
        None::<&str>,
        "/org/whisrs/Overlay",
        "org.whisrs.Overlay",
        "StateChanged",
        &(state.to_string()),
    )
    .await
}

async fn emit_gnome_level(conn: &zbus::Connection, level: f32) -> zbus::Result<()> {
    conn.emit_signal(
        None::<&str>,
        "/org/whisrs/Overlay",
        "org.whisrs.Overlay",
        "LevelChanged",
        &level.clamp(0.0, 1.0),
    )
    .await
}

async fn emit_gnome_theme(conn: &zbus::Connection, theme: &str) -> zbus::Result<()> {
    conn.emit_signal(
        None::<&str>,
        "/org/whisrs/Overlay",
        "org.whisrs.Overlay",
        "ThemeChanged",
        &theme,
    )
    .await
}

async fn emit_gnome_position(
    conn: &zbus::Connection,
    position: OverlayPosition,
) -> zbus::Result<()> {
    conn.emit_signal(
        None::<&str>,
        "/org/whisrs/Overlay",
        "org.whisrs.Overlay",
        "PositionChanged",
        &position.as_str(),
    )
    .await
}

struct GnomeOverlayBus;

#[zbus::interface(name = "org.whisrs.Overlay")]
impl GnomeOverlayBus {
    fn ping(&self) -> &'static str {
        "ok"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::display::AgentVisual;

    #[test]
    fn agents_json_carries_style_corner_and_each_row() {
        let now = Instant::now();
        let entries = [
            AgentOverlayEntry {
                name: "jarvis".into(),
                visual: AgentVisual::Working,
                activity: Some("Running a command".into()),
                since: now - Duration::from_secs(12),
                fading_from: None,
            },
            AgentOverlayEntry {
                name: "friday".into(),
                visual: AgentVisual::NeedsYou,
                activity: None,
                since: now,
                fading_from: None,
            },
        ];
        let json: serde_json::Value = serde_json::from_str(&agents_json(
            &entries,
            AgentOverlayStyle::Face,
            AgentOverlayCorner::TopLeft,
            now,
        ))
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "style": "face",
                "corner": "top-left",
                "agents": [
                    {"name": "jarvis", "state": "working",
                     "activity": "Running a command", "elapsed_s": 12},
                    {"name": "friday", "state": "needs-you",
                     "activity": null, "elapsed_s": 0},
                ],
            })
        );
        let empty: serde_json::Value = serde_json::from_str(&agents_json(
            &[],
            AgentOverlayStyle::Orb,
            AgentOverlayCorner::BottomRight,
            now,
        ))
        .unwrap();
        assert_eq!(
            empty,
            serde_json::json!({"style": "orb", "corner": "bottom-right", "agents": []})
        );
    }
}
