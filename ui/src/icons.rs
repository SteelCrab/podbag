//! Vector icons: SVG path data in a 24 x 24 box, rasterized with zeno at the size drawn
//! and cached in glyphon's atlas as custom glyphs.

use std::sync::OnceLock;

use glyphon::{ContentType, RasterizeCustomGlyphRequest, RasterizedCustomGlyph};
use zeno::{Cap, Format, Join, Mask, Stroke, Transform};

pub struct IconDef {
    /// Stroked path data (24 x 24 box).
    pub stroke: &'static str,
    /// Filled path data, drawn too.
    pub fill: &'static str,
    pub width: f32,
}

static ICONS: OnceLock<Vec<IconDef>> = OnceLock::new();

/// Icons the widgets use themselves, at fixed ids.
pub const CHECK: u16 = 0xF000;
pub const CHEVRON_DOWN: u16 = 0xF001;
pub const CHEVRON_RIGHT: u16 = 0xF002;

const BUILTIN: [IconDef; 3] = [
    IconDef {
        stroke: "M5 12.5 9.5 17 19 7.5",
        fill: "",
        width: 2.6,
    },
    IconDef {
        stroke: "M6 9l6 6 6-6",
        fill: "",
        width: 1.8,
    },
    IconDef {
        stroke: "M9 5l7 7-7 7",
        fill: "",
        width: 1.8,
    },
];

/// Register the app's icons once; an icon's id is its index.
pub fn register(defs: Vec<IconDef>) {
    let defs = defs
        .into_iter()
        .map(|d| IconDef {
            stroke: normalize(d.stroke),
            fill: normalize(d.fill),
            width: d.width,
        })
        .collect();
    let _ = ICONS.set(defs);
}

/// Spell out implicitly repeated commands ("a1 1 0 0 0 1 1 2 2 0 0 0 3 3"), which
/// zeno's parser rejects for arcs. Numbers like ".3" and "1.5.5" are split too.
fn normalize(d: &'static str) -> &'static str {
    if d.is_empty() {
        return d;
    }
    let mut out = String::with_capacity(d.len() + 16);
    let mut cmd = ' ';
    let mut args = 0;
    let mut chars = d.char_indices().peekable();
    while let Some(&(i, c)) = chars.peek() {
        if c.is_ascii_alphabetic() {
            cmd = c;
            args = 0;
            out.push(c);
            chars.next();
            continue;
        }
        if c == ',' || c.is_whitespace() {
            chars.next();
            continue;
        }
        // A number: sign, digits, one dot, digits, exponent.
        let start = i;
        let mut end = i;
        let mut dot = false;
        let mut first = true;
        while let Some(&(j, ch)) = chars.peek() {
            let ok =
                ch.is_ascii_digit() || (ch == '.' && !dot) || ((ch == '-' || ch == '+') && first);
            if !ok {
                break;
            }
            dot |= ch == '.';
            first = false;
            end = j + ch.len_utf8();
            chars.next();
        }
        if end == start {
            chars.next();
            continue;
        }
        let n = match cmd.to_ascii_lowercase() {
            'm' | 'l' | 't' => 2,
            'h' | 'v' => 1,
            'c' => 6,
            's' | 'q' => 4,
            'a' => 7,
            _ => 0,
        };
        if n > 0 && args > 0 && args % n == 0 {
            // A repeat: name the command again (M repeats as L).
            let again = match cmd {
                'M' => 'L',
                'm' => 'l',
                c => c,
            };
            out.push(again);
            cmd = again;
            args = 0;
        } else if args > 0 {
            out.push(' ');
        }
        out.push_str(&d[start..end]);
        args += 1;
    }
    Box::leak(out.into_boxed_str())
}

pub(crate) fn rasterize(req: RasterizeCustomGlyphRequest) -> Option<RasterizedCustomGlyph> {
    let def = match req.id {
        id if id >= CHECK => BUILTIN.get((id - CHECK) as usize)?,
        id => ICONS.get()?.get(id as usize)?,
    };
    let (w, h) = (req.width as u32, req.height as u32);
    if w == 0 || h == 0 {
        return None;
    }
    let scale = w as f32 / 24.;
    let mut data = vec![0u8; (w * h) as usize];
    let transform = Some(Transform::scale(scale, scale));
    if !def.stroke.is_empty() {
        let mut stroke = Stroke::new(def.width);
        stroke.cap(Cap::Round).join(Join::Round);
        let mut buf = vec![0u8; (w * h) as usize];
        Mask::new(def.stroke)
            .style(stroke)
            .transform(transform)
            .format(Format::Alpha)
            .size(w, h)
            .render_into(&mut buf, None);
        merge(&mut data, &buf);
    }
    if !def.fill.is_empty() {
        let mut buf = vec![0u8; (w * h) as usize];
        Mask::new(def.fill)
            .transform(transform)
            .format(Format::Alpha)
            .size(w, h)
            .render_into(&mut buf, None);
        merge(&mut data, &buf);
    }
    Some(RasterizedCustomGlyph {
        data,
        content_type: ContentType::Mask,
    })
}

fn merge(into: &mut [u8], from: &[u8]) {
    for (a, b) in into.iter_mut().zip(from) {
        *a = (*a).max(*b);
    }
}
