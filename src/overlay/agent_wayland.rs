//! The agent overlay's Wayland host: a layer-shell surface of its own in the
//! configured corner, on its own thread and its own connection, apart from
//! the pill's.
//!
//! Idle costs nothing. The loop sleeps in `poll` on the Wayland socket and
//! on a wake socket the board's forwarder writes a byte to per update; with
//! no row shown it requests no frame callback and commits nothing until the
//! board changes. The last frame (transparent) stays attached, so the
//! surface is never unmapped. While a row is shown it draws on frame
//! callbacks, at most every [`FRAME`].

use std::io::{ErrorKind, Read};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{Anchor, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure},
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use tracing::{debug, info, warn};
use wayland_client::{
    backend::WaylandError,
    globals::registry_queue_init,
    protocol::{wl_output, wl_region, wl_shm, wl_surface},
    Connection as WaylandConnection, Dispatch, QueueHandle,
};

use crate::AgentOverlayCorner;

use super::agent_host::{log_pace_change, AgentFrames, ShownFlag, FRAME};
use super::agent_render::{Pace, CANVAS_HEIGHT, CANVAS_WIDTH};
use super::layer::{
    copy_pixmap_to_argb8888, create_layer, layer_action, LayerAction, LayerEvent, LayerStatus,
};
use super::service::OverlayError;

/// The layer surface's namespace, for compositor rules (the pill's is
/// `whisrs`).
const NAMESPACE: &str = "whisrs-agents";

pub(super) fn run_agent_overlay(
    frames: AgentFrames,
    wake: UnixStream,
    corner: AgentOverlayCorner,
    shown: ShownFlag,
) -> Result<(), OverlayError> {
    wake.set_nonblocking(true)?;
    let conn = WaylandConnection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;

    let layer = create_layer(
        &compositor,
        &layer_shell,
        &qh,
        corner_anchor(corner),
        (CANVAS_WIDTH, CANVAS_HEIGHT),
        NAMESPACE,
    );

    let output_state = OutputState::new(&globals, &qh);
    let startup_outputs = output_state.outputs().collect();
    let (w, h) = frames.size();
    let pool = SlotPool::new((w * h * 4) as usize, &shm)?;
    let mut host = AgentOverlay {
        registry_state: RegistryState::new(&globals),
        output_state,
        startup_outputs,
        compositor,
        layer_shell,
        shm,
        pool,
        layer,
        corner,
        frames,
        buffer_scale: 1,
        shown,
        first_configure: true,
        layer_closed: false,
        outputs_changed: false,
        frame_pending: false,
        dirty: true,
        showing: false,
        last_draw: None,
        commits: 0,
    };

    info!("agent overlay started");
    loop {
        event_queue.dispatch_pending(&mut host)?;
        host.tick(&qh);
        if host.frames.disconnected() {
            break;
        }
        match event_queue.flush() {
            Err(WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => {}
            other => other?,
        }
        // Events queued meanwhile are dispatched first.
        let Some(guard) = event_queue.prepare_read() else {
            continue;
        };
        let timeout = host.timeout(Instant::now());
        let (wayland_ready, wake_ready) = wait(guard.connection_fd(), wake.as_fd(), timeout)?;
        if wayland_ready {
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        } else {
            drop(guard);
        }
        // The board's rows are taken by the next `tick`; the bytes only
        // woke the loop. End of file: the forwarder is gone.
        if wake_ready && !drain(&wake)? {
            break;
        }
    }
    info!("agent overlay stopped");
    Ok(())
}

/// The corner's two edges.
fn corner_anchor(corner: AgentOverlayCorner) -> Anchor {
    let vertical = if corner.is_top() {
        Anchor::TOP
    } else {
        Anchor::BOTTOM
    };
    let horizontal = if corner.is_right() {
        Anchor::RIGHT
    } else {
        Anchor::LEFT
    };
    vertical | horizontal
}

/// Wait until the Wayland socket or the wake socket can be read, or
/// `timeout` passes (`None`: no limit). Says which of the two can be read.
fn wait(
    wayland: BorrowedFd<'_>,
    wake: BorrowedFd<'_>,
    timeout: Option<Duration>,
) -> std::io::Result<(bool, bool)> {
    let events = libc::POLLIN;
    let mut fds = [
        libc::pollfd {
            fd: wayland.as_raw_fd(),
            events,
            revents: 0,
        },
        libc::pollfd {
            fd: wake.as_raw_fd(),
            events,
            revents: 0,
        },
    ];
    // Rounded up: a wait cut short would only spin.
    let ms = timeout.map_or(-1, |t| {
        i32::try_from(t.as_micros().div_ceil(1000)).unwrap_or(i32::MAX)
    });
    loop {
        // SAFETY: `fds` is a valid array of `fds.len()` pollfd structs that
        // outlives the call, and both descriptors stay open meanwhile.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
        if n >= 0 {
            break;
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != ErrorKind::Interrupted {
            return Err(err);
        }
    }
    let ready =
        |fd: &libc::pollfd| fd.revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0;
    Ok((ready(&fds[0]), ready(&fds[1])))
}

/// Read every pending wake byte. `false` at end of file.
fn drain(mut wake: &UnixStream) -> std::io::Result<bool> {
    let mut buf = [0_u8; 64];
    loop {
        match wake.read(&mut buf) {
            Ok(0) => return Ok(false),
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(true),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

struct AgentOverlay {
    registry_state: RegistryState,
    output_state: OutputState,
    /// Outputs bound at startup that sctk has not announced yet (see the
    /// pill's `Overlay::startup_outputs`).
    startup_outputs: Vec<wl_output::WlOutput>,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    corner: AgentOverlayCorner,
    frames: AgentFrames,
    /// The surface's integer buffer scale (fractional scales arrive rounded
    /// up as the preferred buffer scale; the compositor scales it down).
    buffer_scale: i32,
    shown: ShownFlag,
    first_configure: bool,
    /// The compositor closed the current layer and no replacement exists yet.
    layer_closed: bool,
    /// An output appeared or went away since the current layer was created.
    outputs_changed: bool,
    /// A frame callback was requested and has not arrived.
    frame_pending: bool,
    /// The rows changed since the last frame, or a new surface waits for
    /// its first.
    dirty: bool,
    /// Rows arrived since the last frame that drew nothing.
    showing: bool,
    last_draw: Option<Instant>,
    /// Commits so far, for the idle debug line.
    commits: u64,
}

impl AgentOverlay {
    /// Take a board update, follow focus when the overlay comes back from
    /// hidden, and draw if a frame is due.
    fn tick(&mut self, qh: &QueueHandle<Self>) {
        if self.frames.update() {
            // Hidden until now: on several outputs a new surface opens on
            // the focused one, like the pill at a session start. Once per
            // showing, however many updates land before the first frame.
            if !self.showing && self.frames.has_rows() {
                self.showing = true;
                self.on_layer_event(qh, LayerEvent::SessionStart, None);
            }
            self.dirty = true;
        }
        let now = Instant::now();
        if self.timeout(now) == Some(Duration::ZERO) {
            self.draw(qh, now);
        }
    }

    /// How long until the next frame is due; `None` when none is, or one
    /// waits on the compositor (a configure, a frame callback).
    fn timeout(&self, now: Instant) -> Option<Duration> {
        if self.layer_closed || self.first_configure || self.frame_pending {
            return None;
        }
        if !self.dirty && self.frames.pace() == Pace::Idle {
            return None;
        }
        Some(self.last_draw.map_or(Duration::ZERO, |at| {
            (at + FRAME).saturating_duration_since(now)
        }))
    }

    /// Swap in a fresh layer surface (see the pill's `recreate_layer`).
    fn recreate_layer(&mut self, qh: &QueueHandle<Self>) {
        self.layer = create_layer(
            &self.compositor,
            &self.layer_shell,
            qh,
            corner_anchor(self.corner),
            (CANVAS_WIDTH, CANVAS_HEIGHT),
            NAMESPACE,
        );
        // No buffer may be attached before the new surface's first
        // configure, which draws. The old surface's callback never comes.
        // A new surface starts at scale 1, and sctk reports a scale only
        // when it differs from 1: the old surface's scale would draw a 1x
        // output at 2x, downscaled.
        self.reset_scale();
        self.first_configure = true;
        self.layer_closed = false;
        self.outputs_changed = false;
        self.frame_pending = false;
        self.dirty = true;
    }

    /// Draw at scale 1, as a new surface starts.
    fn reset_scale(&mut self) {
        match self.frames.set_scale(1.0) {
            Ok(_) => self.buffer_scale = 1,
            Err(e) => warn!(
                "agent overlay stays at scale {} for a new surface: {e}",
                self.buffer_scale
            ),
        }
    }

    /// Outputs still advertised, minus `leaving`.
    fn live_outputs(&self, leaving: Option<&wl_output::WlOutput>) -> usize {
        self.output_state
            .outputs()
            .filter(|output| Some(output) != leaving)
            .count()
    }

    fn layer_status(&self) -> LayerStatus {
        LayerStatus {
            closed: self.layer_closed,
            configured: !self.first_configure,
            outputs_changed: self.outputs_changed,
        }
    }

    fn on_layer_event(
        &mut self,
        qh: &QueueHandle<Self>,
        event: LayerEvent,
        leaving: Option<&wl_output::WlOutput>,
    ) -> LayerAction {
        let action = layer_action(self.layer_status(), event, self.live_outputs(leaving));
        if action == LayerAction::Recreate {
            if self.layer_closed {
                info!("recreating the agent overlay surface the compositor closed");
            } else if self.first_configure {
                info!("recreating the unconfigured agent overlay surface");
            }
            self.recreate_layer(qh);
        }
        action
    }

    fn draw(&mut self, qh: &QueueHandle<Self>, now: Instant) {
        // Set first: a frame that fails is tried again a frame later, not
        // at once.
        self.last_draw = Some(now);
        let prev = self.frames.pace();
        let pace = self.frames.draw(now);
        let (width, height) = self.frames.size();
        let Ok((buffer, canvas)) = self.pool.create_buffer(
            width as i32,
            height as i32,
            width as i32 * 4,
            wl_shm::Format::Argb8888,
        ) else {
            warn!("failed to allocate agent overlay buffer");
            return;
        };
        copy_pixmap_to_argb8888(self.frames.pixmap(), canvas);

        let surface = self.layer.wl_surface();
        surface.set_buffer_scale(self.buffer_scale);
        surface.damage_buffer(0, 0, width as i32, height as i32);
        // Nothing shown: no callback, so nothing runs until the board
        // changes. The transparent buffer stays attached.
        if pace == Pace::Animating {
            surface.frame(qh, surface.clone());
            self.frame_pending = true;
        }
        if let Err(e) = buffer.attach_to(surface) {
            warn!("failed to attach agent overlay buffer: {e}");
            self.frame_pending = false;
            return;
        }
        if pace == Pace::Animating {
            log_pace_change(prev, pace, self.frames.drawn(), self.commits);
        }
        self.layer.commit();
        self.commits += 1;
        if pace == Pace::Idle {
            self.showing = false;
            log_pace_change(prev, pace, self.frames.drawn(), self.commits);
        }
        self.dirty = false;
    }
}

impl CompositorHandler for AgentOverlay {
    fn scale_factor_changed(
        &mut self,
        _conn: &WaylandConnection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        if surface != self.layer.wl_surface() {
            return;
        }
        let scale = new_factor.max(1);
        if scale == self.buffer_scale {
            return;
        }
        // The next frame draws at it: at once while a row is shown, else
        // when the board changes (a hidden overlay commits nothing for it).
        match self.frames.set_scale(scale as f32) {
            Ok(_) => {
                debug!("agent overlay buffer scale {scale}");
                self.buffer_scale = scale;
            }
            Err(e) => warn!("agent overlay stays at scale {}: {e}", self.buffer_scale),
        }
    }

    fn transform_changed(
        &mut self,
        _conn: &WaylandConnection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &WaylandConnection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        // The loop draws once the frame is due. A callback left over from a
        // replaced surface means nothing.
        if surface == self.layer.wl_surface() {
            self.frame_pending = false;
        }
    }

    fn surface_enter(
        &mut self,
        _conn: &WaylandConnection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
    ) {
        let name = self.output_state.info(output).and_then(|info| info.name);
        debug!(
            "agent overlay surface on output {}",
            name.as_deref().unwrap_or("unknown")
        );
    }

    fn surface_leave(
        &mut self,
        _conn: &WaylandConnection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for AgentOverlay {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &WaylandConnection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if let Some(i) = self.startup_outputs.iter().position(|o| *o == output) {
            self.startup_outputs.swap_remove(i);
            return;
        }
        self.outputs_changed = true;
        self.on_layer_event(qh, LayerEvent::OutputAdded, None);
    }

    fn update_output(
        &mut self,
        _conn: &WaylandConnection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &WaylandConnection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.outputs_changed = true;
        self.on_layer_event(qh, LayerEvent::OutputRemoved, Some(&output));
    }
}

impl LayerShellHandler for AgentOverlay {
    fn closed(&mut self, _conn: &WaylandConnection, qh: &QueueHandle<Self>, layer: &LayerSurface) {
        if *layer != self.layer {
            return;
        }
        self.layer_closed = true;
        self.frame_pending = false;
        // Not on screen until a surface is configured again: agent toasts
        // stand in meanwhile.
        self.shown.clear();
        if self.on_layer_event(qh, LayerEvent::Closed, None) == LayerAction::Keep {
            info!("agent overlay surface closed by the compositor, waiting for an output change");
        }
    }

    fn configure(
        &mut self,
        _conn: &WaylandConnection,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        _configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        if *layer != self.layer {
            return;
        }
        if self.first_configure {
            self.first_configure = false;
            // The first buffer maps the surface: a transparent one when no
            // row is shown. On screen (again, after a close).
            self.draw(qh, Instant::now());
            self.shown.set();
        }
    }
}

impl ShmHandler for AgentOverlay {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl Dispatch<wl_region::WlRegion, ()> for AgentOverlay {
    fn event(
        _state: &mut Self,
        _proxy: &wl_region::WlRegion,
        _event: wl_region::Event,
        _data: &(),
        _conn: &WaylandConnection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

delegate_compositor!(AgentOverlay);
delegate_output!(AgentOverlay);
delegate_shm!(AgentOverlay);
delegate_layer!(AgentOverlay);
delegate_registry!(AgentOverlay);

impl ProvidesRegistryState for AgentOverlay {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState];
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn corner_anchor_pins_two_edges() {
        use AgentOverlayCorner::*;
        assert_eq!(corner_anchor(BottomRight), Anchor::BOTTOM | Anchor::RIGHT);
        assert_eq!(corner_anchor(BottomLeft), Anchor::BOTTOM | Anchor::LEFT);
        assert_eq!(corner_anchor(TopRight), Anchor::TOP | Anchor::RIGHT);
        assert_eq!(corner_anchor(TopLeft), Anchor::TOP | Anchor::LEFT);
    }

    #[test]
    fn a_wake_byte_ends_the_wait_and_end_of_file_ends_the_loop() {
        let (a, b) = UnixStream::pair().unwrap();
        let (idle, _keep) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        // Nothing written: the wait times out with nothing ready.
        assert_eq!(
            wait(idle.as_fd(), a.as_fd(), Some(Duration::from_millis(5))).unwrap(),
            (false, false)
        );
        (&b).write_all(&[1, 1, 1]).unwrap();
        assert_eq!(wait(idle.as_fd(), a.as_fd(), None).unwrap(), (false, true));
        assert!(drain(&a).unwrap(), "bytes drained, the forwarder lives");
        assert_eq!(
            wait(idle.as_fd(), a.as_fd(), Some(Duration::ZERO)).unwrap(),
            (false, false)
        );
        drop(b);
        assert_eq!(wait(idle.as_fd(), a.as_fd(), None).unwrap(), (false, true));
        assert!(!drain(&a).unwrap(), "end of file");
    }
}
