//! PNGs of the agent overlay, for comparing with the mockups.
//!
//! ```sh
//! cargo run --example agent_overlay_frames -- <out-dir>
//! ```
//!
//! For each style (`orb`, `face`) it writes:
//! - `strip-<style>-<state>.png`: one row in four animation phases, at 2x;
//! - `sheet-<style>@1x.png` and `@2x`: every state (top to bottom:
//!   listening, transcribing, working, speaking, needs you, done, error)
//!   by phase (left to right);
//! - `scene-<style>-<corner>@1x.png` and `@2x`: four rows in each corner;
//! - `check-<style>@2x.png`, `entrance-<style>@2x.png`,
//!   `fade-<style>@2x.png`: a turn finishing (Working to Done, the check
//!   drawing itself), a row arriving, and a Done row fading out;
//!
//! plus `scene-more@2x.png` (six agents: three rows and "+3 more", long
//! text cut to fit) and `scene-light@2x.png` (on a light desktop). Every
//! frame is composited on a desktop color so the cards' edges show.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tiny_skia::{Color, IntRect, Pixmap, PixmapPaint, Transform};
use whisrs::agent::display::{rows, AgentOverlayEntry, AgentVisual, DONE_FADE};
use whisrs::overlay::{canvas_size, AgentScene, CANVAS_HEIGHT, CANVAS_WIDTH, ENTRANCE};
use whisrs::{AgentOverlayCorner, AgentOverlayStyle};

const DARK: [u8; 3] = [0x0B, 0x0D, 0x11];
const LIGHT: [u8; 3] = [0xE9, 0xEC, 0xF1];

/// The first row's slot at the bottom of the canvas, with room for what
/// a shape draws past its box: logical y and height.
const ROW_CROP: (u32, u32) = (168, 68);

const STATES: [AgentVisual; 7] = [
    AgentVisual::Listening,
    AgentVisual::Transcribing,
    AgentVisual::Working,
    AgentVisual::Speaking,
    AgentVisual::NeedsYou,
    AgentVisual::Done,
    AgentVisual::Error,
];

/// Seconds into a state, and the voice level then.
const PHASES: [(f32, f32); 4] = [(0.0, 0.2), (0.3, 0.9), (0.6, 0.55), (0.9, 0.05)];

const CORNERS: [AgentOverlayCorner; 4] = [
    AgentOverlayCorner::BottomRight,
    AgentOverlayCorner::BottomLeft,
    AgentOverlayCorner::TopRight,
    AgentOverlayCorner::TopLeft,
];

fn main() -> Result<(), Box<dyn Error>> {
    let out = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("usage: agent_overlay_frames <out-dir>")?,
    );
    std::fs::create_dir_all(&out)?;
    let base = Instant::now() + Duration::from_secs(3600);

    for style in [AgentOverlayStyle::Orb, AgentOverlayStyle::Face] {
        let name = style.as_str();
        let mut sheets = [Vec::new(), Vec::new()];
        for visual in STATES {
            for (scale, sheet) in [1.0, 2.0].into_iter().zip(&mut sheets) {
                sheet.push(phases(style, visual, base, scale));
            }
            let strip = grid(&[phases(style, visual, base, 2.0)]);
            let state = visual.as_str();
            save(&strip, &out, &format!("strip-{name}-{state}.png"))?;
        }
        save(&grid(&sheets[0]), &out, &format!("sheet-{name}@1x.png"))?;
        save(&grid(&sheets[1]), &out, &format!("sheet-{name}@2x.png"))?;

        for corner in CORNERS {
            for scale in [1.0, 2.0] {
                let pm = scene(style, corner, &four_rows(base), base, scale, DARK);
                let s = scale as u32;
                save(
                    &pm,
                    &out,
                    &format!("scene-{name}-{}@{s}x.png", corner.as_str()),
                )?;
            }
        }

        save(
            &grid(&[check(style, base)]),
            &out,
            &format!("check-{name}@2x.png"),
        )?;
        save(
            &grid(&[entrance(style, base)]),
            &out,
            &format!("entrance-{name}@2x.png"),
        )?;
        save(
            &grid(&[fade(style, base)]),
            &out,
            &format!("fade-{name}@2x.png"),
        )?;
    }

    let corner = AgentOverlayCorner::BottomRight;
    let more = scene(
        AgentOverlayStyle::Orb,
        corner,
        &six_rows(base),
        base,
        2.0,
        DARK,
    );
    save(&more, &out, "scene-more@2x.png")?;
    let light = scene(
        AgentOverlayStyle::Face,
        corner,
        &four_rows(base),
        base,
        2.0,
        LIGHT,
    );
    save(&light, &out, "scene-light@2x.png")?;
    println!("wrote {}", out.display());
    Ok(())
}

fn entry(
    name: &str,
    visual: AgentVisual,
    activity: Option<&str>,
    since: Instant,
) -> AgentOverlayEntry {
    AgentOverlayEntry {
        name: name.to_string(),
        visual,
        activity: activity.map(str::to_string),
        since,
        fading_from: None,
    }
}

fn ago(base: Instant, secs: u64) -> Instant {
    base - Duration::from_secs(secs)
}

/// One agent in `visual`, with the activity line it would have.
fn sample(visual: AgentVisual, base: Instant) -> AgentOverlayEntry {
    let (activity, since) = match visual {
        AgentVisual::Listening | AgentVisual::Transcribing => (None, ago(base, 4)),
        AgentVisual::Working | AgentVisual::Speaking => (Some("Reading your inbox"), ago(base, 42)),
        AgentVisual::NeedsYou => (Some("Waiting for your approval"), ago(base, 58)),
        AgentVisual::Done => (Some("Finished in 1:24"), ago(base, 84)),
        AgentVisual::Error => (Some("Failed after 0:12"), ago(base, 12)),
    };
    entry("jarvis", visual, activity, since)
}

fn four_rows(base: Instant) -> Vec<AgentOverlayEntry> {
    vec![
        entry(
            "jarvis",
            AgentVisual::Speaking,
            Some("Reading your inbox"),
            ago(base, 3),
        ),
        entry(
            "friday",
            AgentVisual::Working,
            Some("Running a command"),
            ago(base, 42),
        ),
        entry(
            "research",
            AgentVisual::NeedsYou,
            Some("Waiting for your approval"),
            ago(base, 118),
        ),
        entry(
            "notes",
            AgentVisual::Done,
            Some("Finished in 1:24"),
            ago(base, 84),
        ),
    ]
}

fn six_rows(base: Instant) -> Vec<AgentOverlayEntry> {
    let mut entries = vec![
        entry("jarvis", AgentVisual::Listening, None, ago(base, 2)),
        entry(
            "documentation-writer-agent",
            AgentVisual::Working,
            Some("Searching the code for the overlay renderer and its tests"),
            ago(base, 3725),
        ),
        entry(
            "friday",
            AgentVisual::Error,
            Some("Failed after 0:12"),
            ago(base, 12),
        ),
    ];
    for name in ["a", "b", "c"] {
        entries.push(entry(name, AgentVisual::Working, None, ago(base, 1)));
    }
    entries
}

/// A whole canvas, the rows settled (they appeared long before `now`),
/// on a desktop color.
fn scene(
    style: AgentOverlayStyle,
    corner: AgentOverlayCorner,
    entries: &[AgentOverlayEntry],
    now: Instant,
    scale: f32,
    desktop: [u8; 3],
) -> Pixmap {
    let mut scene = AgentScene::new(style, corner);
    let mut pm = canvas(scale);
    scene.draw(
        &mut pm,
        rows(entries),
        now - Duration::from_secs(10),
        0.6,
        scale,
    );
    scene.draw(&mut pm, rows(entries), now, 0.6, scale);
    on_desktop(&pm, desktop)
}

/// One row in `visual` at each of [`PHASES`], settled in the state.
fn phases(style: AgentOverlayStyle, visual: AgentVisual, base: Instant, scale: f32) -> Vec<Pixmap> {
    let entries = [sample(visual, base)];
    let mut scene = AgentScene::new(style, AgentOverlayCorner::BottomRight);
    let mut pm = canvas(scale);
    // In the state for a while: the loops run, a one-shot draw-in is done.
    let settled = base + Duration::from_secs(8);
    scene.draw(&mut pm, rows(&entries), base, 0.0, scale);
    PHASES
        .iter()
        .map(|(t, level)| {
            let now = settled + Duration::from_secs_f32(*t);
            scene.draw(&mut pm, rows(&entries), now, *level, scale);
            row_of(&pm, scale)
        })
        .collect()
}

/// A turn finishing: Working, then Done with the check drawing itself.
fn check(style: AgentOverlayStyle, base: Instant) -> Vec<Pixmap> {
    let mut scene = AgentScene::new(style, AgentOverlayCorner::BottomRight);
    let mut pm = canvas(2.0);
    let working = [sample(AgentVisual::Working, base)];
    scene.draw(
        &mut pm,
        rows(&working),
        base - Duration::from_secs(2),
        0.0,
        2.0,
    );
    let done = [sample(AgentVisual::Done, base)];
    [0.0, 0.06, 0.12, 0.2, 0.3]
        .iter()
        .map(|t| {
            scene.draw(
                &mut pm,
                rows(&done),
                base + Duration::from_secs_f32(*t),
                0.0,
                2.0,
            );
            row_of(&pm, 2.0)
        })
        .collect()
}

/// A new row arriving over [`ENTRANCE`].
fn entrance(style: AgentOverlayStyle, base: Instant) -> Vec<Pixmap> {
    let mut scene = AgentScene::new(style, AgentOverlayCorner::BottomRight);
    let mut pm = canvas(2.0);
    let entries = [sample(AgentVisual::Listening, base)];
    (0..=4)
        .map(|i| {
            let now = base + ENTRANCE * i / 4;
            scene.draw(&mut pm, rows(&entries), now, 0.4, 2.0);
            row_of(&pm, 2.0)
        })
        .collect()
}

/// A Done row fading out over [`DONE_FADE`].
fn fade(style: AgentOverlayStyle, base: Instant) -> Vec<Pixmap> {
    let mut scene = AgentScene::new(style, AgentOverlayCorner::BottomRight);
    let mut pm = canvas(2.0);
    let mut done = sample(AgentVisual::Done, base);
    done.fading_from = Some(base);
    let entries = [done];
    scene.draw(
        &mut pm,
        rows(&entries),
        base - Duration::from_secs(5),
        0.0,
        2.0,
    );
    (0..=4)
        .map(|i| {
            let now = base + DONE_FADE * i / 4;
            scene.draw(&mut pm, rows(&entries), now, 0.0, 2.0);
            row_of(&pm, 2.0)
        })
        .collect()
}

fn canvas(scale: f32) -> Pixmap {
    let (w, h) = canvas_size(scale);
    Pixmap::new(w, h).expect("canvas size is non-zero")
}

/// The first row's slot of a bottom-right canvas, on the dark desktop.
fn row_of(pm: &Pixmap, scale: f32) -> Pixmap {
    let s = scale as u32;
    let (y, h) = ROW_CROP;
    let rect = IntRect::from_xywh(0, (y * s) as i32, CANVAS_WIDTH * s, h * s).expect("crop rect");
    let row = pm.clone_rect(rect).expect("crop inside the canvas");
    assert_eq!(y + h, CANVAS_HEIGHT);
    on_desktop(&row, DARK)
}

fn on_desktop(overlay: &Pixmap, desktop: [u8; 3]) -> Pixmap {
    let mut out = Pixmap::new(overlay.width(), overlay.height()).expect("non-zero size");
    out.fill(Color::from_rgba8(desktop[0], desktop[1], desktop[2], 255));
    out.draw_pixmap(
        0,
        0,
        overlay.as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        None,
    );
    out
}

/// Cells laid out in rows, one pixel line of the desktop between them.
fn grid(cells: &[Vec<Pixmap>]) -> Pixmap {
    let cell_w = cells[0][0].width();
    let cell_h = cells[0][0].height();
    let cols = cells.iter().map(Vec::len).max().unwrap_or(1) as u32;
    let w = cols * cell_w + (cols - 1);
    let h = cells.len() as u32 * cell_h + (cells.len() as u32 - 1);
    let mut out = Pixmap::new(w, h).expect("non-zero size");
    out.fill(Color::from_rgba8(0x30, 0x34, 0x3C, 255));
    for (r, row) in cells.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            let x = c as u32 * (cell_w + 1);
            let y = r as u32 * (cell_h + 1);
            out.draw_pixmap(
                x as i32,
                y as i32,
                cell.as_ref(),
                &PixmapPaint::default(),
                Transform::identity(),
                None,
            );
        }
    }
    out
}

fn save(pm: &Pixmap, dir: &Path, name: &str) -> Result<(), Box<dyn Error>> {
    pm.save_png(dir.join(name))?;
    Ok(())
}
