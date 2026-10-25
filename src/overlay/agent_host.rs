//! What the agent overlay's Wayland and X11 hosts share: the board and the
//! voice level they read, the scene and pixmap they draw, and the counts
//! that show an idle overlay costs nothing.
//!
//! The host thread reads both watch channels itself (a `borrow` needs no
//! runtime). The board's forwarder ([`super::service`]) only wakes it when
//! the rows change; the level is read at each frame, and frames are drawn
//! only while a row is shown, so a level change needs no wake of its own.

use std::time::{Duration, Instant};

use tiny_skia::Pixmap;
use tokio::sync::watch;
use tracing::debug;

use crate::agent::display::{rows, AgentOverlayEntry};
use crate::{AgentOverlayCorner, AgentOverlayStyle};

use super::agent_render::{canvas_size, AgentScene, Pace};
use super::service::OverlayError;

/// The frame interval while a row is shown: about 30 fps.
pub(super) const FRAME: Duration = Duration::from_millis(33);

/// The voice level's smoothing time constant, seconds: the shapes follow
/// the voice without the jitter of single level samples.
const LEVEL_SMOOTHING: f32 = 0.08;

/// The rows to draw, and what was drawn last.
pub(super) struct AgentFrames {
    entries_rx: watch::Receiver<Vec<AgentOverlayEntry>>,
    level_rx: watch::Receiver<f32>,
    entries: Vec<AgentOverlayEntry>,
    scene: AgentScene,
    pixmap: Pixmap,
    scale: f32,
    /// The smoothed voice level, and when it was last stepped.
    level: f32,
    level_at: Option<Instant>,
    /// What the last frame said: `Idle` once it drew nothing.
    pace: Pace,
    /// Frames drawn so far.
    drawn: u64,
    /// The board is gone (the daemon is shutting down).
    disconnected: bool,
}

impl AgentFrames {
    /// Frames for the board's rows at scale 1, starting with what it shows
    /// now.
    pub(super) fn new(
        mut entries_rx: watch::Receiver<Vec<AgentOverlayEntry>>,
        level_rx: watch::Receiver<f32>,
        style: AgentOverlayStyle,
        corner: AgentOverlayCorner,
    ) -> Result<Self, OverlayError> {
        let entries = entries_rx.borrow_and_update().clone();
        Ok(Self {
            entries_rx,
            level_rx,
            entries,
            scene: AgentScene::new(style, corner),
            pixmap: new_pixmap(1.0)?,
            scale: 1.0,
            level: 0.0,
            level_at: None,
            pace: Pace::Idle,
            drawn: 0,
            disconnected: false,
        })
    }

    /// Take the board's rows if it published since the last call. `true`
    /// when it did.
    pub(super) fn update(&mut self) -> bool {
        match self.entries_rx.has_changed() {
            Ok(true) => {
                self.entries = self.entries_rx.borrow_and_update().clone();
                true
            }
            Ok(false) => false,
            Err(_) => {
                self.disconnected = true;
                false
            }
        }
    }

    /// The board is gone: the host should end.
    pub(super) fn disconnected(&self) -> bool {
        self.disconnected
    }

    /// The board has a row to show.
    pub(super) fn has_rows(&self) -> bool {
        !self.entries.is_empty()
    }

    /// What the last frame said (`Idle` before the first).
    pub(super) fn pace(&self) -> Pace {
        self.pace
    }

    /// Frames drawn so far.
    pub(super) fn drawn(&self) -> u64 {
        self.drawn
    }

    /// Draw at `scale` from now on (a new pixmap). `false` when it is the
    /// current one.
    pub(super) fn set_scale(&mut self, scale: f32) -> Result<bool, OverlayError> {
        if scale == self.scale {
            return Ok(false);
        }
        self.pixmap = new_pixmap(scale)?;
        self.scale = scale;
        Ok(true)
    }

    /// The pixmap's size, physical px.
    pub(super) fn size(&self) -> (u32, u32) {
        (self.pixmap.width(), self.pixmap.height())
    }

    pub(super) fn pixmap(&self) -> &Pixmap {
        &self.pixmap
    }

    /// Draw the rows as of `now`, and say whether to keep drawing.
    pub(super) fn draw(&mut self, now: Instant) -> Pace {
        let target = self.level_rx.borrow().clamp(0.0, 1.0);
        let dt = self
            .level_at
            .map_or(0.0, |at| now.saturating_duration_since(at).as_secs_f32());
        // A first frame, or one after a pause, starts at the level as it is.
        self.level = if self.pace == Pace::Idle || dt > 0.25 {
            target
        } else {
            self.level + (target - self.level) * (1.0 - (-dt / LEVEL_SMOOTHING).exp())
        };
        self.level_at = Some(now);
        self.pace = self.scene.draw(
            &mut self.pixmap,
            rows(&self.entries),
            now,
            self.level,
            self.scale,
        );
        self.drawn += 1;
        self.pace
    }
}

/// Log a host's frame going from `prev` to `pace`, with how many frames it
/// sent so far (`sent`: Wayland commits, X11 images). Called before a
/// drawing frame is sent and after an idle one: the count at "drawing"
/// equals the one at the "idle" before it, so an idle overlay sent nothing.
pub(super) fn log_pace_change(prev: Pace, pace: Pace, drawn: u64, sent: u64) {
    if prev == pace {
        return;
    }
    let what = match pace {
        Pace::Animating => "drawing",
        Pace::Idle => "idle",
    };
    debug!("agent overlay {what}: {drawn} frames drawn, {sent} sent");
}

fn new_pixmap(scale: f32) -> Result<Pixmap, OverlayError> {
    let (w, h) = canvas_size(scale);
    Pixmap::new(w, h).ok_or(OverlayError::Pixmap(w, h))
}

/// Says the agent overlay is on screen ([`super::AgentOverlayShown`]):
/// `set` once the host's window exists, `clear` while it has none (the
/// compositor closed it); false again when dropped, as the host ends.
pub(super) struct ShownFlag(watch::Sender<bool>);

impl ShownFlag {
    pub(super) fn new(tx: watch::Sender<bool>) -> Self {
        Self(tx)
    }

    pub(super) fn set(&self) {
        self.0
            .send_if_modified(|shown| !std::mem::replace(shown, true));
    }

    pub(super) fn clear(&self) {
        self.0
            .send_if_modified(|shown| std::mem::replace(shown, false));
    }
}

impl Drop for ShownFlag {
    fn drop(&mut self) {
        self.0
            .send_if_modified(|shown| std::mem::replace(shown, false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::display::AgentVisual;

    fn entry(name: &str) -> AgentOverlayEntry {
        AgentOverlayEntry {
            name: name.into(),
            visual: AgentVisual::Working,
            activity: Some("Running a command".into()),
            since: Instant::now(),
            fading_from: None,
        }
    }

    fn frames() -> (
        watch::Sender<Vec<AgentOverlayEntry>>,
        watch::Sender<f32>,
        AgentFrames,
    ) {
        let (board_tx, board_rx) = watch::channel(Vec::new());
        let (level_tx, level_rx) = watch::channel(0.0);
        let frames = AgentFrames::new(
            board_rx,
            level_rx,
            AgentOverlayStyle::Orb,
            AgentOverlayCorner::BottomRight,
        )
        .unwrap();
        (board_tx, level_tx, frames)
    }

    #[test]
    fn idle_until_the_board_has_a_row_and_again_once_it_is_empty() {
        let (board, _level, mut f) = frames();
        assert!(!f.update());
        assert_eq!(f.pace(), Pace::Idle);
        board.send_replace(vec![entry("jarvis")]);
        assert!(f.update());
        assert!(!f.update(), "a publication is taken once");
        assert!(f.has_rows());
        assert_eq!(f.draw(Instant::now()), Pace::Animating);
        board.send_replace(Vec::new());
        assert!(f.update());
        assert_eq!(f.draw(Instant::now()), Pace::Idle);
        assert!(f.pixmap().pixels().iter().all(|px| px.alpha() == 0));
        assert_eq!(f.drawn(), 2);
    }

    #[test]
    fn a_dropped_board_disconnects() {
        let (board, _level, mut f) = frames();
        drop(board);
        assert!(!f.update());
        assert!(f.disconnected());
    }

    #[test]
    fn a_new_scale_resizes_the_pixmap() {
        let (_board, _level, mut f) = frames();
        assert_eq!(f.size(), canvas_size(1.0));
        assert!(f.set_scale(2.0).unwrap());
        assert!(!f.set_scale(2.0).unwrap());
        assert_eq!(f.size(), canvas_size(2.0));
        assert_eq!(f.scale, 2.0);
    }

    #[test]
    fn the_level_is_smoothed_between_frames() {
        let (board, level, mut f) = frames();
        board.send_replace(vec![entry("jarvis")]);
        f.update();
        let t0 = Instant::now();
        f.draw(t0);
        level.send_replace(1.0);
        f.draw(t0 + FRAME);
        assert!(f.level > 0.0 && f.level < 1.0, "{}", f.level);
        f.draw(t0 + FRAME * 10);
        assert!(f.level > 0.95, "{}", f.level);
    }

    #[test]
    fn the_shown_flag_clears_when_the_host_ends() {
        let (tx, rx) = watch::channel(false);
        let flag = ShownFlag::new(tx);
        flag.set();
        assert!(*rx.borrow());
        drop(flag);
        assert!(!*rx.borrow());
    }

    /// A surface the compositor closed is off screen until the next one is
    /// configured: the flag follows, and only real changes are published.
    #[test]
    fn the_shown_flag_clears_while_the_surface_is_closed() {
        let (tx, mut rx) = watch::channel(false);
        let flag = ShownFlag::new(tx);
        flag.set();
        assert!(rx.has_changed().unwrap());
        assert!(*rx.borrow_and_update());
        flag.clear();
        assert!(!*rx.borrow_and_update());
        flag.clear();
        assert!(!rx.has_changed().unwrap(), "cleared twice, published once");
        flag.set();
        assert!(*rx.borrow_and_update());
    }

    /// A new surface draws at scale 1 again (the Wayland host resets it):
    /// the pixmap is back to its 1x size.
    #[test]
    fn a_scale_goes_back_to_one() {
        let (_board, _level, mut f) = frames();
        f.set_scale(2.0).unwrap();
        assert!(f.set_scale(1.0).unwrap());
        assert_eq!(f.size(), canvas_size(1.0));
    }
}
