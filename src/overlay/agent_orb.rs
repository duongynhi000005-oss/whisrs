//! The orb style: one ring, one color and one motion per state, in a row's
//! 44 px box (the "A, Orb" mockup scaled from 140 px).

use std::f32::consts::{PI, TAU};

use tiny_skia::{Pixmap, StrokeDash, Transform};

use crate::agent::display::AgentVisual;

use super::agent_render::{
    arc, circle, ease_out_cubic, fill, line, state_color, stroke, stroke_dashed, wave, ShapeFrame,
    SHAPE,
};
use super::text::{self, TextStyle, Weight};

/// The box's center.
const C: f32 = SHAPE / 2.0;
/// The outer ring's radius, to the middle of its stroke.
const RING: f32 = 20.0;
const RING_W: f32 = 2.2;
/// A breathing or rippling ring's stroke.
const THIN_W: f32 = 1.8;
/// How much of a turn a working arc covers.
const ARC: f32 = 0.85 * PI;
const BANG: TextStyle = TextStyle::new(Weight::SemiBold, 22.0);

pub(super) fn draw(pixmap: &mut Pixmap, visual: AgentVisual, f: &ShapeFrame) {
    let rgb = state_color(visual);
    let ts = f.transform();
    let a = f.alpha;
    let ring = |pixmap: &mut Pixmap, r: f32, alpha: f32, width: f32| {
        if let Some(path) = circle(C, C, r) {
            stroke(pixmap, &path, rgb, alpha, width, ts);
        }
    };
    match visual {
        AgentVisual::Listening => {
            // Two rings breathe over 1.6 s, the inner one 0.35 s behind;
            // how far they swell follows the mic.
            let swell = 0.06 + 0.14 * f.level;
            for (radius, delay, opacity) in [(RING, 0.0, 1.0), (15.0, 0.35, 0.6)] {
                let b = wave(f.t - delay, 1.6);
                let alpha = a * opacity * (0.5 + 0.5 * b);
                ring(pixmap, radius * (0.92 + swell * b), alpha, THIN_W);
            }
            core(pixmap, rgb, a, 6.0 + 1.5 * f.level, ts);
        }
        AgentVisual::Transcribing => {
            // A dashed ring turning once per 1.2 s. Fourteen whole dash
            // periods around it, so there is no seam.
            let around = TAU * RING;
            let period = around / 14.0;
            let offset = (-(f.t / 1.2).fract() * around).rem_euclid(around);
            let dash = StrokeDash::new(vec![period * 0.55, period * 0.45], offset);
            if let Some(path) = circle(C, C, RING) {
                stroke_dashed(pixmap, &path, rgb, a, 2.4, dash, ts);
            }
            core(pixmap, rgb, a * 0.9, 5.5, ts);
        }
        AgentVisual::Working => {
            // Two arcs turning opposite ways, at 2.4 s and 3.4 s a turn.
            let outer = -0.75 * PI + TAU * f.t / 2.4;
            let inner = 0.25 * PI - TAU * f.t / 3.4;
            for (r, start, alpha) in [(RING, outer, a), (14.5, inner, a * 0.7)] {
                if let Some(path) = arc(C, C, r, start, ARC) {
                    stroke(pixmap, &path, rgb, alpha, 2.4, ts);
                }
            }
            core(pixmap, rgb, a, 5.0, ts);
        }
        AgentVisual::Speaking => {
            // Three ripples 0.6 s apart, each spreading and fading over
            // 1.8 s; how far and how bright follows the voice.
            let reach = 0.45 + 0.3 * f.level;
            let bright = 0.55 + 0.45 * f.level;
            for k in 0..3 {
                let p = ((f.t - 0.6 * k as f32) / 1.8).rem_euclid(1.0);
                let alpha = a * 0.95 * (1.0 - p) * bright;
                ring(pixmap, 19.5 * (0.55 + reach * p), alpha, THIN_W);
            }
            core(pixmap, rgb, a, 6.5 + 1.5 * f.level, ts);
        }
        AgentVisual::NeedsYou => {
            // The ring blinks once a second; the "!" holds.
            let blink = 1.0 - 0.7 * wave(f.t, 1.0);
            ring(pixmap, RING, a * blink, RING_W);
            text::draw_centered(pixmap, '!', BANG, f.at(C, C), rgb, a, f.scale);
        }
        AgentVisual::Done => {
            ring(pixmap, RING, a, RING_W);
            check(pixmap, rgb, a, (f.t / 0.3).clamp(0.0, 1.0), ts);
        }
        AgentVisual::Error => {
            ring(pixmap, RING, a, RING_W);
            let d = 5.5;
            for (x0, x1) in [(C - d, C + d), (C + d, C - d)] {
                if let Some(path) = line(x0, C - d, x1, C + d) {
                    stroke(pixmap, &path, rgb, a, 2.4, ts);
                }
            }
        }
    }
}

fn core(pixmap: &mut Pixmap, rgb: [u8; 3], alpha: f32, r: f32, ts: Transform) {
    if let Some(path) = circle(C, C, r) {
        fill(pixmap, &path, rgb, alpha, ts);
    }
}

/// The mockup's check (`M5 12.5l4.5 4.5L19 7.5` in a 24 px box), centered,
/// drawn up to `progress` (0 to 1) of its length.
fn check(pixmap: &mut Pixmap, rgb: [u8; 3], alpha: f32, progress: f32, ts: Transform) {
    const K: f32 = 0.8;
    let o = C - 12.0 * K;
    let p = |x: f32, y: f32| (o + x * K, o + y * K);
    let pts = [p(5.0, 12.5), p(9.5, 17.0), p(19.0, 7.5)];
    let mut pb = tiny_skia::PathBuilder::new();
    pb.move_to(pts[0].0, pts[0].1);
    pb.line_to(pts[1].0, pts[1].1);
    pb.line_to(pts[2].0, pts[2].1);
    let Some(path) = pb.finish() else {
        return;
    };
    let shown = ease_out_cubic(progress);
    if shown >= 1.0 {
        stroke(pixmap, &path, rgb, alpha, 2.4, ts);
        return;
    }
    let length: f32 = pts
        .windows(2)
        .map(|w| ((w[1].0 - w[0].0).powi(2) + (w[1].1 - w[0].1).powi(2)).sqrt())
        .sum();
    if shown * length > 0.1 {
        let dash = StrokeDash::new(vec![shown * length, length * 2.0], 0.0);
        stroke_dashed(pixmap, &path, rgb, alpha, 2.4, dash, ts);
    }
}
