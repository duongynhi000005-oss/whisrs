//! What the pill's and the agent overlay's layer-shell surfaces share:
//! creating one, deciding when to replace it, and copying a frame into a
//! shm buffer.

use smithay_client_toolkit::{
    compositor::{CompositorState, SurfaceData},
    reexports::protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerSurface, LayerSurfaceData,
        },
        WaylandSurface,
    },
};
use tiny_skia::{Pixmap, PremultipliedColorU8};
use wayland_client::{
    protocol::{wl_region, wl_surface},
    Dispatch, QueueHandle,
};

use super::render::EDGE_MARGIN;

/// Create a click-through layer surface of `size` (logical px) on the
/// overlay layer, pinned to `anchor`'s edges `EDGE_MARGIN` away. Output
/// `None` lets the compositor place it, normally on the focused output.
/// Layer-shell cannot move a surface to another output, so following focus
/// means a new surface.
pub(super) fn create_layer<D>(
    compositor: &CompositorState,
    layer_shell: &LayerShell,
    qh: &QueueHandle<D>,
    anchor: Anchor,
    (width, height): (u32, u32),
    namespace: &str,
) -> LayerSurface
where
    D: Dispatch<wl_surface::WlSurface, SurfaceData>
        + Dispatch<ZwlrLayerSurfaceV1, LayerSurfaceData>
        + Dispatch<wl_region::WlRegion, ()>
        + 'static,
{
    let surface = compositor.create_surface(qh);
    let layer =
        layer_shell.create_layer_surface(qh, surface, Layer::Overlay, Some(namespace), None);
    layer.set_anchor(anchor);
    // Margins on unanchored edges are ignored, so set all four.
    layer.set_margin(EDGE_MARGIN, EDGE_MARGIN, EDGE_MARGIN, EDGE_MARGIN);
    layer.set_exclusive_zone(0);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.set_size(width, height);

    // Make the transparent overlay non-interactive so it never blocks clicks.
    let input_region = compositor.wl_compositor().create_region(qh, ());
    layer.set_input_region(Some(&input_region));
    input_region.destroy();

    layer.commit();
    layer
}

/// Where the current layer surface stands, as far as recovery cares.
#[derive(Clone, Copy, Debug)]
pub(super) struct LayerStatus {
    /// The compositor closed it.
    pub(super) closed: bool,
    /// Its first configure arrived.
    pub(super) configured: bool,
    /// An output appeared or went away since it was created.
    pub(super) outputs_changed: bool,
}

/// An event that may call for a new layer surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LayerEvent {
    /// The compositor closed the current layer.
    Closed,
    /// An output appeared.
    OutputAdded,
    /// An output is going away.
    OutputRemoved,
    /// The overlay is about to show after being hidden: a recording
    /// session started (the pill), or an agent row appeared on an empty
    /// agent overlay.
    SessionStart,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LayerAction {
    /// Replace the current layer with a fresh one.
    Recreate,
    /// Keep the current layer: it is fine, or recovery waits for an output
    /// change.
    Keep,
}

/// Decide whether `event` should replace the current layer.
/// `live_outputs` leaves out an output that is going away.
pub(super) fn layer_action(
    status: LayerStatus,
    event: LayerEvent,
    live_outputs: usize,
) -> LayerAction {
    // Nothing can hold a new surface. The next output to appear retries.
    if live_outputs == 0 {
        return LayerAction::Keep;
    }
    let recreate = match event {
        // A close before the first configure means the compositor placed the
        // surface on an output it is tearing down. Retrying at once could
        // land there again, so wait for an output to come or go, unless one
        // already did since this surface was created.
        LayerEvent::Closed => status.configured || status.outputs_changed,
        // Output changes are finite, so retrying on them cannot loop. Retry a
        // layer that was never configured as well as a closed one: Hyprland
        // sends neither configure nor closed to a surface it created while
        // no monitor was left.
        LayerEvent::OutputAdded => status.closed || !status.configured,
        // Only a closed layer. Sway sends `closed` and the output's removal
        // in one batch, so a layer not configured yet is likely the
        // replacement `closed` just made. If that one is closed before its
        // first configure, `outputs_changed` lets the `Closed` arm retry.
        LayerEvent::OutputRemoved => status.closed,
        // With several outputs, a new surface follows focus. With one there
        // is nothing to follow, and a fresh map would replay the compositor's
        // layer open animation on every session. A dead layer is no reason
        // either: it draws no frames, so nothing wakes the loop when a
        // session starts. It waits for the next output change.
        LayerEvent::SessionStart => live_outputs > 1,
    };
    if recreate {
        LayerAction::Recreate
    } else {
        LayerAction::Keep
    }
}

/// tiny-skia stores premultiplied RGBA bytes; the wl_shm Argb8888 format on
/// little-endian systems is BGRA in memory. Convert by swapping R and B.
/// Both formats use premultiplied alpha so no math is needed beyond the swap.
pub(super) fn copy_pixmap_to_argb8888(pixmap: &Pixmap, canvas: &mut [u8]) {
    let src = pixmap.pixels();
    debug_assert_eq!(src.len() * 4, canvas.len());
    for (i, px) in src.iter().enumerate() {
        let dst = &mut canvas[i * 4..i * 4 + 4];
        let pre: PremultipliedColorU8 = *px;
        dst[0] = pre.blue();
        dst[1] = pre.green();
        dst[2] = pre.red();
        dst[3] = pre.alpha();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use LayerAction::{Keep, Recreate};
    use LayerEvent::{Closed, OutputAdded, OutputRemoved, SessionStart};

    /// Configured and showing.
    const OPEN: LayerStatus = LayerStatus {
        closed: false,
        configured: true,
        outputs_changed: false,
    };
    /// Configured, then closed by the compositor.
    const CLOSED: LayerStatus = LayerStatus {
        closed: true,
        ..OPEN
    };
    /// Closed before its first configure.
    const CLOSED_UNCONFIGURED: LayerStatus = LayerStatus {
        configured: false,
        ..CLOSED
    };
    /// Neither configured nor closed.
    const UNCONFIGURED: LayerStatus = LayerStatus {
        configured: false,
        ..OPEN
    };

    /// The output handlers mark the change before they decide.
    fn changed(status: LayerStatus) -> LayerStatus {
        LayerStatus {
            outputs_changed: true,
            ..status
        }
    }

    #[test]
    fn closed_after_configure_recreates_while_an_output_remains() {
        assert_eq!(layer_action(CLOSED, Closed, 1), Recreate);
    }

    #[test]
    fn closed_with_no_outputs_waits() {
        assert_eq!(layer_action(CLOSED, Closed, 0), Keep);
    }

    #[test]
    fn closed_before_configure_waits_for_an_output_change() {
        assert_eq!(layer_action(CLOSED_UNCONFIGURED, Closed, 1), Keep);
    }

    #[test]
    fn closed_before_configure_after_an_output_change_recreates() {
        assert_eq!(
            layer_action(changed(CLOSED_UNCONFIGURED), Closed, 1),
            Recreate
        );
        assert_eq!(layer_action(changed(CLOSED_UNCONFIGURED), Closed, 0), Keep);
    }

    #[test]
    fn output_added_revives_a_closed_layer() {
        assert_eq!(layer_action(changed(CLOSED), OutputAdded, 1), Recreate);
        assert_eq!(
            layer_action(changed(CLOSED_UNCONFIGURED), OutputAdded, 1),
            Recreate
        );
    }

    #[test]
    fn output_added_retries_a_layer_never_configured() {
        assert_eq!(
            layer_action(changed(UNCONFIGURED), OutputAdded, 1),
            Recreate
        );
    }

    #[test]
    fn output_removed_with_outputs_left_retries_a_closed_layer() {
        assert_eq!(layer_action(changed(CLOSED), OutputRemoved, 1), Recreate);
        assert_eq!(
            layer_action(changed(CLOSED_UNCONFIGURED), OutputRemoved, 1),
            Recreate
        );
    }

    #[test]
    fn output_removed_keeps_a_fresh_unconfigured_replacement() {
        for live in [1, 2] {
            assert_eq!(
                layer_action(changed(UNCONFIGURED), OutputRemoved, live),
                Keep
            );
        }
    }

    #[test]
    fn output_removed_leaving_no_outputs_waits() {
        for status in [CLOSED, CLOSED_UNCONFIGURED, UNCONFIGURED, OPEN] {
            assert_eq!(layer_action(changed(status), OutputRemoved, 0), Keep);
        }
    }

    #[test]
    fn output_change_leaves_an_open_layer_alone() {
        for event in [OutputAdded, OutputRemoved] {
            for live in [1, 2] {
                assert_eq!(layer_action(changed(OPEN), event, live), Keep);
            }
        }
    }

    #[test]
    fn hyprland_unplug_of_the_only_monitor_recovers_on_replug() {
        // `closed` lands before `global_remove`, so the leaving monitor still
        // counts and a replacement goes up at once.
        assert_eq!(layer_action(CLOSED, Closed, 1), Recreate);
        // Hyprland never configures or closes that replacement. Then the
        // monitor's global goes away.
        assert_eq!(layer_action(changed(UNCONFIGURED), OutputRemoved, 0), Keep);
        // Replugging retries it.
        assert_eq!(
            layer_action(changed(UNCONFIGURED), OutputAdded, 1),
            Recreate
        );
    }

    #[test]
    fn sway_unplug_with_a_monitor_left_recreates_once() {
        // `closed` and `global_remove` arrive in one batch. At `closed` the
        // leaving output still counts, and a replacement goes up.
        assert_eq!(layer_action(CLOSED, Closed, 2), Recreate);
        // The removal then finds that replacement not configured yet.
        assert_eq!(layer_action(changed(UNCONFIGURED), OutputRemoved, 1), Keep);
    }

    #[test]
    fn replacement_closed_before_configure_after_an_output_removal_recreates() {
        assert_eq!(layer_action(CLOSED, Closed, 2), Recreate);
        assert_eq!(layer_action(changed(UNCONFIGURED), OutputRemoved, 1), Keep);
        // The removal came after the replacement was made, so its early close
        // is not the one it was waiting for.
        assert_eq!(
            layer_action(changed(CLOSED_UNCONFIGURED), Closed, 1),
            Recreate
        );
    }

    #[test]
    fn session_start_keeps_a_healthy_layer_on_one_output() {
        assert_eq!(layer_action(OPEN, SessionStart, 1), Keep);
    }

    #[test]
    fn session_start_with_several_outputs_follows_focus() {
        for live in [2, 3] {
            assert_eq!(layer_action(OPEN, SessionStart, live), Recreate);
        }
    }

    #[test]
    fn session_start_leaves_a_dead_layer_on_one_output_alone() {
        // A dead layer draws no frames, so in practice no session start
        // reaches it. It waits for an output change instead.
        for status in [CLOSED, CLOSED_UNCONFIGURED, UNCONFIGURED] {
            assert_eq!(layer_action(status, SessionStart, 1), Keep);
        }
    }

    #[test]
    fn session_start_after_a_replug_keeps_the_fresh_layer() {
        // A session that started while no monitor was left is still flagged
        // when a monitor comes back. The replug already made a fresh layer,
        // so the flag must not replace it again.
        assert_eq!(
            layer_action(changed(UNCONFIGURED), OutputAdded, 1),
            Recreate
        );
        assert_eq!(layer_action(UNCONFIGURED, SessionStart, 1), Keep);
    }

    #[test]
    fn session_start_with_no_outputs_waits() {
        for status in [CLOSED, CLOSED_UNCONFIGURED, UNCONFIGURED, OPEN] {
            assert_eq!(layer_action(status, SessionStart, 0), Keep);
        }
    }

    #[test]
    fn removal_before_close_then_early_close_recovers_on_the_next_output_change() {
        // `global_remove` lands first and leaves the open layer alone.
        assert_eq!(layer_action(changed(OPEN), OutputRemoved, 1), Keep);
        // Then the old layer is closed, and a replacement goes up.
        assert_eq!(layer_action(changed(CLOSED), Closed, 1), Recreate);
        // The compositor closes the replacement before its first configure.
        // The output change it would wait for already happened, so it stays
        // down until the next one. A known gap: no compositor is known to
        // order events this way.
        assert_eq!(layer_action(CLOSED_UNCONFIGURED, Closed, 1), Keep);
        assert_eq!(layer_action(CLOSED_UNCONFIGURED, SessionStart, 1), Keep);
        for event in [OutputAdded, OutputRemoved] {
            assert_eq!(
                layer_action(changed(CLOSED_UNCONFIGURED), event, 1),
                Recreate
            );
        }
    }

    #[test]
    fn argb8888_swaps_red_and_blue() {
        let mut pixmap = Pixmap::new(1, 1).unwrap();
        pixmap.fill(tiny_skia::Color::from_rgba8(0x10, 0x20, 0x30, 0xFF));
        let mut canvas = [0_u8; 4];
        copy_pixmap_to_argb8888(&pixmap, &mut canvas);
        assert_eq!(canvas, [0x30, 0x20, 0x10, 0xFF]);
    }
}
