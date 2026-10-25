//! The agent overlay's X11 host: an override-redirect ARGB window like the
//! pill's, in the configured corner of the monitor holding the active
//! window (else the primary one), shaped to what it draws and
//! click-through. It draws at `Xft.dpi / 96` (1 to 3).
//!
//! Idle costs nothing: with no row shown the window is unmapped and the
//! thread blocks on its wake channel until the board changes. While a row
//! is shown it redraws every [`FRAME`], at once for a board update.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Instant;

use tiny_skia::Pixmap;
use tracing::{debug, info, warn};
use x11rb::connection::{Connection as X11Connection, RequestConnection};
use x11rb::protocol::{
    shape::{ConnectionExt as X11ShapeConnectionExt, SK, SO},
    xproto::{
        AtomEnum, ClipOrdering, ColormapAlloc, ConfigureWindowAux,
        ConnectionExt as X11ProtoConnectionExt, CreateGCAux, CreateWindowAux, EventMask,
        ImageFormat, Rectangle, StackMode, Window, WindowClass,
    },
};
use x11rb::rust_connection::RustConnection;

use crate::{AgentOverlayCorner, OverlayPosition};

use super::agent_host::{log_pace_change, AgentFrames, ShownFlag, FRAME};
use super::agent_render::Pace;
use super::render::EDGE_MARGIN;
use super::service::OverlayError;
use super::x11::{
    copy_pixmap_to_x11_zpixmap, find_argb_visual, make_x11_window_click_through, screen_position,
    set_x11_bounding_shape, set_x11_window_hints, shape_rectangles_eq, x11_overlay_position,
    X11Atoms,
};

pub(super) fn run_agent_x11_overlay(
    mut frames: AgentFrames,
    wake: mpsc::Receiver<()>,
    corner: AgentOverlayCorner,
    shown: ShownFlag,
) -> Result<(), OverlayError> {
    let (conn, screen_num) = RustConnection::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let visual = find_argb_visual(screen).ok_or(OverlayError::X11Visual)?;
    let atoms = X11Atoms::new(&conn)?;

    let scale = scale_for_dpi(xft_dpi(&conn, screen.root));
    frames.set_scale(scale)?;
    let (w, h) = frames.size();
    let size = (
        u16::try_from(w).map_err(|_| OverlayError::Pixmap(w, h))?,
        u16::try_from(h).map_err(|_| OverlayError::Pixmap(w, h))?,
    );
    let margin = (EDGE_MARGIN as f32 * scale).round() as i32;
    let position = corner_position(corner);

    let colormap = conn.generate_id()?;
    conn.create_colormap(ColormapAlloc::NONE, colormap, screen.root, visual.visual_id)?;

    let (x, y) = screen_position(screen, size.0, size.1, position, margin);
    let window = conn.generate_id()?;
    conn.create_window(
        visual.depth,
        window,
        screen.root,
        x,
        y,
        size.0,
        size.1,
        0,
        WindowClass::INPUT_OUTPUT,
        visual.visual_id,
        &CreateWindowAux::default()
            .background_pixel(0)
            .border_pixel(0)
            .colormap(colormap)
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE),
    )?;

    set_x11_window_hints(&conn, window, &atoms, b"whisrs agents")?;
    let shape_supported = make_x11_window_click_through(&conn, window)?;

    let gc = conn.generate_id()?;
    conn.create_gc(gc, window, &CreateGCAux::default().graphics_exposures(0))?;
    conn.flush()?;
    shown.set();

    let mut image = vec![0_u8; w as usize * h as usize * 4];
    let mut mapped = false;
    let mut last_shape: Vec<Rectangle> = Vec::new();
    let mut last_draw: Option<Instant> = None;
    // Images sent so far, for the idle debug line.
    let mut sent = 0_u64;
    // The board's rows at startup are drawn without waiting for a change.
    let mut startup = true;

    info!("agent X11 overlay started (scale {scale})");
    loop {
        // Redraws are time-driven; events are only drained.
        while conn.poll_for_event()?.is_some() {}

        let updated = frames.update() || std::mem::take(&mut startup);
        if frames.disconnected() {
            break;
        }
        if updated || frames.pace() == Pace::Animating {
            let now = Instant::now();
            let prev = frames.pace();
            let pace = frames.draw(now);
            last_draw = Some(now);
            if pace == Pace::Idle {
                if mapped {
                    // Clear what the shape still holds, so a later map
                    // shows nothing of this showing (see below).
                    copy_pixmap_to_x11_zpixmap(frames.pixmap(), visual, &mut image);
                    put_image(&conn, window, gc, size, visual.depth, &image)?;
                    sent += 1;
                    conn.unmap_window(window)?;
                    conn.flush()?;
                    mapped = false;
                }
                log_pace_change(prev, pace, frames.drawn(), sent);
            } else {
                log_pace_change(prev, pace, frames.drawn(), sent);
                let shape = alpha_band_rectangles(frames.pixmap());
                let reshape = shape_supported && !shape_rectangles_eq(&shape, &last_shape);
                copy_pixmap_to_x11_zpixmap(frames.pixmap(), visual, &mut image);
                if !mapped {
                    let (x, y) =
                        x11_overlay_position(&conn, screen, &atoms, size, position, margin);
                    debug!("agent X11 overlay mapped at {x},{y}");
                    conn.configure_window(
                        window,
                        &ConfigureWindowAux::default()
                            .x(i32::from(x))
                            .y(i32::from(y))
                            .stack_mode(StackMode::ABOVE),
                    )?;
                    conn.map_window(window)?;
                    mapped = true;
                }
                // An image only lands inside the bounding shape, and a
                // server that composites the whole window (Xwayland) shows
                // what is left outside it. So the shape grows to the old
                // and the new one, the frame clears what leaves it, and
                // only then does it shrink.
                if reshape {
                    grow_x11_bounding_shape(&conn, window, &shape)?;
                }
                put_image(&conn, window, gc, size, visual.depth, &image)?;
                if reshape {
                    set_x11_bounding_shape(&conn, window, &shape)?;
                    last_shape = shape;
                }
                conn.flush()?;
                sent += 1;
            }
        }

        // Idle: block until the board changes. Shown: until the next frame,
        // or a board update.
        let woke = match last_draw.filter(|_| frames.pace() == Pace::Animating) {
            Some(at) => {
                let wait = (at + FRAME).saturating_duration_since(Instant::now());
                !matches!(wake.recv_timeout(wait), Err(RecvTimeoutError::Disconnected))
            }
            None => wake.recv().is_ok(),
        };
        if !woke {
            break;
        }
        // One update takes every wake queued meanwhile.
        while wake.try_recv().is_ok() {}
    }

    if mapped {
        let _ = conn.unmap_window(window);
        let _ = conn.flush();
    }
    info!("agent X11 overlay stopped");
    Ok(())
}

/// The pill's placement for the corner.
fn corner_position(corner: AgentOverlayCorner) -> OverlayPosition {
    match corner {
        AgentOverlayCorner::BottomRight => OverlayPosition::BottomRight,
        AgentOverlayCorner::BottomLeft => OverlayPosition::BottomLeft,
        AgentOverlayCorner::TopRight => OverlayPosition::TopRight,
        AgentOverlayCorner::TopLeft => OverlayPosition::TopLeft,
    }
}

/// `Xft.dpi` from the root window's resource database (`xrdb`), if set.
fn xft_dpi(conn: &RustConnection, root: Window) -> Option<f32> {
    let reply = conn
        .get_property(
            false,
            root,
            AtomEnum::RESOURCE_MANAGER,
            AtomEnum::STRING,
            0,
            1 << 16,
        )
        .ok()?
        .reply()
        .map_err(|e| warn!("failed to read the X resource database: {e}"))
        .ok()?;
    parse_xft_dpi(&String::from_utf8_lossy(&reply.value))
}

/// The `Xft.dpi` value in an X resource database.
fn parse_xft_dpi(resources: &str) -> Option<f32> {
    resources
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            if key.trim() != "Xft.dpi" {
                return None;
            }
            value.trim().parse::<f32>().ok()
        })
        .filter(|dpi| dpi.is_finite() && *dpi > 0.0)
}

/// Pixmap px per logical px: `dpi / 96`, 1 to 3; 1 without a dpi.
fn scale_for_dpi(dpi: Option<f32>) -> f32 {
    dpi.map_or(1.0, |dpi| (dpi / 96.0).clamp(1.0, 3.0))
}

/// The bounding shape for `pixmap`: per pixel row, the runs of pixels with
/// any alpha, with consecutive rows whose runs match merged into one band
/// (a card's straight sides are one rectangle, not one per row). Sorted by
/// y.
fn alpha_band_rectangles(pixmap: &Pixmap) -> Vec<Rectangle> {
    let width = pixmap.width() as usize;
    let mut rectangles: Vec<Rectangle> = Vec::new();
    // Where the previous row's rectangles start, and its runs.
    let mut band = 0;
    let mut prev: Vec<(usize, usize)> = Vec::new();
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (y, row) in pixmap.pixels().chunks_exact(width).enumerate() {
        runs.clear();
        let mut x = 0;
        while x < width {
            if row[x].alpha() == 0 {
                x += 1;
                continue;
            }
            let start = x;
            while x < width && row[x].alpha() != 0 {
                x += 1;
            }
            runs.push((start, x - start));
        }
        if !runs.is_empty() && runs == prev {
            for rect in &mut rectangles[band..] {
                rect.height += 1;
            }
        } else {
            band = rectangles.len();
            rectangles.extend(runs.iter().map(|&(x, w)| Rectangle {
                x: x as i16,
                y: y as i16,
                width: w as u16,
                height: 1,
            }));
        }
        std::mem::swap(&mut prev, &mut runs);
    }
    rectangles
}

/// Add `rectangles` to the window's bounding shape.
fn grow_x11_bounding_shape(
    conn: &RustConnection,
    window: Window,
    rectangles: &[Rectangle],
) -> Result<(), OverlayError> {
    conn.shape_rectangles(
        SO::UNION,
        SK::BOUNDING,
        ClipOrdering::Y_SORTED,
        window,
        0,
        0,
        rectangles,
    )?;
    Ok(())
}

/// Send `image` (Z-pixmap rows of `size`), in as many requests as the
/// server's request size limit asks for.
fn put_image(
    conn: &RustConnection,
    window: Window,
    gc: u32,
    (width, height): (u16, u16),
    depth: u8,
    image: &[u8],
) -> Result<(), OverlayError> {
    let stride = usize::from(width) * 4;
    // The request's own header is 24 bytes.
    let rows = ((conn.maximum_request_bytes().saturating_sub(24)) / stride.max(1)).max(1);
    let mut y = 0_usize;
    while y < usize::from(height) {
        let n = rows.min(usize::from(height) - y);
        conn.put_image(
            ImageFormat::Z_PIXMAP,
            window,
            gc,
            width,
            n as u16,
            0,
            y as i16,
            0,
            depth,
            &image[y * stride..(y + n) * stride],
        )?;
        y += n;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiny_skia::{Color, Paint, Rect, Transform};

    fn rect(x: i16, y: i16, width: u16, height: u16) -> Rectangle {
        Rectangle {
            x,
            y,
            width,
            height,
        }
    }

    fn fill(pixmap: &mut Pixmap, x: f32, y: f32, w: f32, h: f32) {
        let mut paint = Paint::default();
        paint.set_color(Color::WHITE);
        paint.anti_alias = false;
        pixmap.fill_rect(
            Rect::from_xywh(x, y, w, h).unwrap(),
            &paint,
            Transform::identity(),
            None,
        );
    }

    fn eq(a: &[Rectangle], b: &[Rectangle]) -> bool {
        shape_rectangles_eq(a, b)
    }

    #[test]
    fn alpha_bands_merge_matching_rows_and_split_gaps() {
        let mut pixmap = Pixmap::new(8, 6).unwrap();
        // A 3x3 block, and beside its first row a pixel; a gap row; a bar.
        fill(&mut pixmap, 1.0, 0.0, 3.0, 3.0);
        fill(&mut pixmap, 6.0, 0.0, 1.0, 1.0);
        fill(&mut pixmap, 0.0, 4.0, 8.0, 2.0);
        let shape = alpha_band_rectangles(&pixmap);
        let want = [
            rect(1, 0, 3, 1),
            rect(6, 0, 1, 1),
            rect(1, 1, 3, 2),
            rect(0, 4, 8, 2),
        ];
        assert!(eq(&shape, &want), "{shape:?}");
        // Sorted by y, as the shape request says.
        assert!(shape.windows(2).all(|p| p[0].y <= p[1].y));
    }

    #[test]
    fn a_transparent_pixmap_has_no_shape() {
        let pixmap = Pixmap::new(8, 6).unwrap();
        assert!(alpha_band_rectangles(&pixmap).is_empty());
    }

    #[test]
    fn xft_dpi_is_read_from_the_resource_database() {
        let db = "Xcursor.size:\t24\nXft.antialias:\t1\nXft.dpi:\t192\n";
        assert_eq!(parse_xft_dpi(db), Some(192.0));
        assert_eq!(parse_xft_dpi("Xft.dpi: 144.5"), Some(144.5));
        assert_eq!(parse_xft_dpi("Xcursor.size:\t24\n"), None);
        assert_eq!(parse_xft_dpi("Xft.dpi:\tlots\n"), None);
        assert_eq!(parse_xft_dpi("Xft.dpi:\t0\n"), None);
    }

    #[test]
    fn the_scale_follows_the_dpi_within_one_to_three() {
        assert_eq!(scale_for_dpi(None), 1.0);
        assert_eq!(scale_for_dpi(Some(96.0)), 1.0);
        assert_eq!(scale_for_dpi(Some(144.0)), 1.5);
        assert_eq!(scale_for_dpi(Some(192.0)), 2.0);
        assert_eq!(scale_for_dpi(Some(72.0)), 1.0);
        assert_eq!(scale_for_dpi(Some(480.0)), 3.0);
    }

    #[test]
    fn every_corner_maps_to_the_pill_position_of_that_corner() {
        use AgentOverlayCorner::*;
        for corner in [BottomRight, BottomLeft, TopRight, TopLeft] {
            let position = corner_position(corner);
            assert_eq!(position.as_str(), corner.as_str());
        }
    }
}
