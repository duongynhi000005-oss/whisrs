//! The face style: a rounded square whose eyes and mouth carry the state,
//! in a row's 44 px box (the "B, Face" mockup scaled from 140 px).

use std::f32::consts::PI;

use tiny_skia::Pixmap;

use crate::agent::display::AgentVisual;

use super::agent_render::{
    arc, circle, fill, line, rounded_rect, state_color, stroke, wave, ShapeFrame, SHAPE,
};
use super::text::{self, TextStyle, Weight};

/// The box's center.
const C: f32 = SHAPE / 2.0;
/// The face's square inside the box, its corner radius and outline.
const FACE_AT: f32 = 3.0;
const FACE: f32 = 38.0;
const FACE_R: f32 = 12.0;
const OUTLINE: f32 = 1.6;
const FACE_BG: [u8; 3] = [0x1E, 0x22, 0x2B];
/// A drawn feature's line: mouth ring, brow, ^^ eyes, smile, x eyes.
const FEATURE_W: f32 = 1.4;
/// The needs-you badge's "?" and its color.
const BADGE_MARK: TextStyle = TextStyle::new(Weight::SemiBold, 10.0);
const BADGE_INK: [u8; 3] = [0x0E, 0x10, 0x14];

/// A mockup coordinate (the face's 116 px content box) in box px.
fn u(v: f32) -> f32 {
    4.0 + v * (36.0 / 116.0)
}

/// A mockup length in box px.
fn len(v: f32) -> f32 {
    v * (36.0 / 116.0)
}

pub(super) fn draw(pixmap: &mut Pixmap, visual: AgentVisual, f: &ShapeFrame) {
    let rgb = state_color(visual);
    let a = f.alpha;
    // A waiting face bobs over 1.6 s.
    let bob = if visual == AgentVisual::NeedsYou {
        -1.3 * wave(f.t, 1.6)
    } else {
        0.0
    };
    let ts = f.transform().pre_translate(0.0, bob);
    let dot = |pixmap: &mut Pixmap, x: f32, y: f32, r: f32, alpha: f32| {
        if let Some(path) = circle(x, y, r) {
            fill(pixmap, &path, rgb, alpha, ts);
        }
    };
    let bar = |pixmap: &mut Pixmap, x0: f32, x1: f32, y: f32, h: f32| {
        if let Some(path) = rounded_rect(x0, y - h / 2.0, x1 - x0, h, h / 2.0) {
            fill(pixmap, &path, rgb, a, ts);
        }
    };
    let draw_line = |pixmap: &mut Pixmap, (x0, y0): (f32, f32), (x1, y1): (f32, f32)| {
        if let Some(path) = line(x0, y0, x1, y1) {
            stroke(pixmap, &path, rgb, a, FEATURE_W, ts);
        }
    };
    let draw_arc = |pixmap: &mut Pixmap, (cx, cy): (f32, f32), r: f32, start: f32, sweep: f32| {
        if let Some(path) = arc(cx, cy, r, start, sweep) {
            stroke(pixmap, &path, rgb, a, FEATURE_W, ts);
        }
    };

    if visual == AgentVisual::Listening {
        // The outline breathes around the face over 1.6 s; how far follows
        // the mic.
        let b = wave(f.t, 1.6);
        let s = 0.92 + (0.10 + 0.08 * f.level) * b;
        let breathe = ts.pre_translate(C, C).pre_scale(s, s).pre_translate(-C, -C);
        if let Some(path) = rounded_rect(0.6, 0.6, SHAPE - 1.2, SHAPE - 1.2, 14.0) {
            stroke(pixmap, &path, rgb, a * (0.35 + 0.55 * b), 1.2, breathe);
        }
    }
    if let Some(path) = rounded_rect(FACE_AT, FACE_AT, FACE, FACE, FACE_R) {
        fill(pixmap, &path, FACE_BG, a, ts);
        stroke(pixmap, &path, rgb, a, OUTLINE, ts);
    }

    match visual {
        AgentVisual::Listening => {
            // Eyes wide, mouth open (a little wider with the voice).
            dot(pixmap, u(36.0), u(48.0), len(10.0), a);
            dot(pixmap, u(80.0), u(48.0), len(10.0), a);
            if let Some(path) = circle(u(59.0), u(83.0), 2.1 + 0.8 * f.level) {
                stroke(pixmap, &path, rgb, a, FEATURE_W, ts);
            }
        }
        AgentVisual::Transcribing => {
            // Pupils read left to right and back, 1.1 s each way.
            let scan = len(-9.0 + 18.0 * wave(f.t, 2.2));
            dot(pixmap, u(37.0) + scan, u(49.0), len(7.0), a);
            dot(pixmap, u(79.0) + scan, u(49.0), len(7.0), a);
            bar(pixmap, u(40.0), u(76.0), u(86.0), 1.5);
        }
        AgentVisual::Working => {
            // Pupils up and to the side, three dots bouncing 0.2 s apart.
            dot(pixmap, u(41.0), u(39.0), len(7.0), a);
            dot(pixmap, u(87.0), u(39.0), len(7.0), a);
            bar(pixmap, u(46.0), u(70.0), u(86.0), 1.5);
            for i in 0..3 {
                let b = wave(f.t - 0.2 * i as f32, 1.2);
                let x = 32.7 + 4.4 * i as f32;
                dot(pixmap, x, 1.0 - 1.6 * b, 1.5, a * (0.25 + 0.75 * b));
            }
        }
        AgentVisual::Speaking => {
            dot(pixmap, u(36.0), u(46.0), len(8.0), a);
            dot(pixmap, u(80.0), u(46.0), len(8.0), a);
            // Four mouth bars, their height following the voice.
            let (w, gap) = (1.8, 1.4);
            let x0 = u(58.0) - (4.0 * w + 3.0 * gap) / 2.0;
            for i in 0..4 {
                let move_ = 0.45 + 0.55 * wave(f.t - 0.12 * i as f32, 0.5);
                let h = 1.9 + 5.0 * f.level * move_;
                let x = x0 + i as f32 * (w + gap);
                if let Some(path) = rounded_rect(x, u(84.0) - h / 2.0, w, h, w / 2.0) {
                    fill(pixmap, &path, rgb, a, ts);
                }
            }
        }
        AgentVisual::NeedsYou => {
            // One raised brow, a tilted mouth, a "?" badge on the corner.
            let tilt = |cx: f32, cy: f32, half: f32, deg: f32| {
                let (s, c) = deg.to_radians().sin_cos();
                (
                    (cx - c * half, cy - s * half),
                    (cx + c * half, cy + s * half),
                )
            };
            let (b0, b1) = tilt(u(35.0), u(27.5), len(11.0), -14.0);
            draw_line(pixmap, b0, b1);
            dot(pixmap, u(36.0), u(48.0), len(8.0), a);
            dot(pixmap, u(80.0), u(48.0), len(8.0), a);
            let (m0, m1) = tilt(u(58.0), u(86.0), len(14.0), -6.0);
            draw_line(pixmap, m0, m1);
            let badge = (38.0, 6.0);
            dot(pixmap, badge.0, badge.1, 6.0, a);
            let center = f.at(badge.0, badge.1 + bob);
            text::draw_centered(pixmap, '?', BADGE_MARK, center, BADGE_INK, a, f.scale);
        }
        AgentVisual::Done => {
            // ^^ and a smile.
            for x in [u(39.0), u(77.0)] {
                draw_arc(pixmap, (x, u(52.0)), 3.6, PI, PI);
            }
            draw_arc(pixmap, (u(59.0), u(76.0)), 5.8, 0.0, PI);
        }
        AgentVisual::Error => {
            // x eyes, a small frown.
            let d = 2.4;
            for x in [u(36.0), u(80.0)] {
                let y = u(48.0);
                draw_line(pixmap, (x - d, y - d), (x + d, y + d));
                draw_line(pixmap, (x + d, y - d), (x - d, y + d));
            }
            draw_arc(pixmap, (C, u(96.0)), 4.5, 1.2 * PI, 0.6 * PI);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mockup_coordinates_fill_the_face() {
        // The mockup's 116 px content box maps inside the face's outline.
        assert!(u(0.0) >= FACE_AT + OUTLINE / 2.0);
        assert!(u(116.0) <= FACE_AT + FACE - OUTLINE / 2.0 + 0.5);
    }
}
