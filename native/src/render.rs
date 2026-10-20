//! CPU compositing of a window's surfaces into its Android surface.

use crate::Caption;
use crate::android::Frame;
use crate::output::to_px;
use crate::shm::Texture;
use crate::state::State;

const BACKGROUND: u32 = 0xff20_2020;

#[inline]
fn blend(src: u32, dst: u32) -> u32 {
    let a = src >> 24;
    if a == 255 {
        return src;
    }
    if a == 0 {
        return dst;
    }
    let inv = 255 - a;
    let rb = ((dst & 0x00ff00ff) * inv + 0x00800080) >> 8 & 0x00ff00ff;
    let ag = ((dst >> 8) & 0x00ff00ff) * inv + 0x00800080 & 0xff00ff00;
    src.wrapping_add(rb | ag)
}

/// Interpolates two premultiplied RGBA pixels, `f` in 0..=256.
#[inline]
fn lerp(a: u32, b: u32, f: u32) -> u32 {
    let g = 256 - f;
    let rb = ((a & 0x00ff00ff) * g + (b & 0x00ff00ff) * f) >> 8 & 0x00ff00ff;
    let ag = ((a >> 8) & 0x00ff00ff) * g + ((b >> 8) & 0x00ff00ff) * f & 0xff00ff00;
    rb | ag
}

/// Draws the `src` rect of `t` (texture px) into the `dst` rect of the
/// frame (x, y, w, h px). Equal sizes copy pixels 1:1 (what fractional-scale
/// clients produce); anything else is scaled bilinearly.
fn blit(frame: &mut Frame, t: &Texture, src: [f64; 4], dst: [i32; 4], opaque_copy: bool) {
    let [dx, dy, dw, dh] = dst;
    if dw <= 0 || dh <= 0 || src[2] <= 0.0 || src[3] <= 0.0 || t.width <= 0 || t.height <= 0 {
        return;
    }
    let (x0, y0) = (dx.max(0), dy.max(0));
    let (x1, y1) = ((dx + dw).min(frame.width), (dy + dh).min(frame.height));
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let stride = frame.stride as usize;
    let tw = t.width as usize;
    let put = |d: &mut u32, s: u32| *d = if opaque_copy { s } else { blend(s, *d) };
    let exact = src[2] as i32 == dw && src[3] as i32 == dh && src[0].fract() == 0.0 && src[1].fract() == 0.0;
    if exact {
        let (sx, sy) = (src[0] as i32, src[1] as i32);
        for y in y0..y1 {
            let row = (sy + y - dy) as usize * tw;
            let s = &t.data[row + (sx + x0 - dx) as usize..row + (sx + x1 - dx) as usize];
            let d = &mut frame.pixels[y as usize * stride + x0 as usize..y as usize * stride + x1 as usize];
            if opaque_copy {
                d.copy_from_slice(s);
            } else {
                for (d, s) in d.iter_mut().zip(s) {
                    *d = blend(*s, *d);
                }
            }
        }
        return;
    }
    // Sample at pixel centres; 8 fractional bits per axis.
    let (sxs, sys) = (src[2] / dw as f64, src[3] / dh as f64);
    let (maxx, maxy) = (t.width as f64 - 1.0, t.height as f64 - 1.0);
    let cols: Vec<(usize, usize, u32)> = (x0..x1)
        .map(|x| {
            let fx = (src[0] + (x - dx) as f64 * sxs + sxs / 2.0 - 0.5).clamp(0.0, maxx);
            let ix = fx as usize;
            (ix, (ix + 1).min(tw - 1), ((fx - ix as f64) * 256.0) as u32)
        })
        .collect();
    for y in y0..y1 {
        let fy = (src[1] + (y - dy) as f64 * sys + sys / 2.0 - 0.5).clamp(0.0, maxy);
        let iy = fy as usize;
        let (r0, r1) = (iy * tw, (iy + 1).min(t.height as usize - 1) * tw);
        let wy = ((fy - iy as f64) * 256.0) as u32;
        let d = &mut frame.pixels[y as usize * stride + x0 as usize..y as usize * stride + x1 as usize];
        for (d, &(a, b, wx)) in d.iter_mut().zip(&cols) {
            let top = lerp(t.data[r0 + a], t.data[r0 + b], wx);
            let bottom = lerp(t.data[r1 + a], t.data[r1 + b], wx);
            put(d, lerp(top, bottom, wy));
        }
    }
}

/// Mean luma of the caption strip under the system controls.
fn caption_is_light(frame: &Frame, c: &Caption) -> Option<bool> {
    if c.height <= 0 {
        return None;
    }
    let rects = &c.controls;
    let (mut sum, mut n) = (0u64, 0u64);
    let y = (c.top + c.height / 2).clamp(0, frame.height - 1);
    let row = &frame.pixels[y as usize * frame.stride as usize..][..frame.width as usize];
    let spans: Vec<(i32, i32)> = if rects.is_empty() {
        vec![(frame.width * 3 / 4, frame.width)]
    } else {
        rects.iter().map(|r| (r[0], r[0] + r[2])).collect()
    };
    for (a, b) in spans {
        for x in (a.max(0)..b.min(frame.width)).step_by(4) {
            let v = row[x as usize];
            let (r, g, b) = (v & 0xff, (v >> 8) & 0xff, (v >> 16) & 0xff);
            sum += (r * 299 + g * 587 + b * 114) as u64 / 1000;
            n += 1;
        }
    }
    (n > 0).then(|| sum / n > 140)
}

/// Splits the caption band into what the app must receive (anything drawn:
/// icons, labels, entries, hovered buttons) and empty header the system may
/// drag the window by. A column is empty if it is one uniform colour from
/// top to bottom of the band. Returns exclusion rects in px.
fn header_exclusion(frame: &Frame, c: &Caption) -> Vec<[i32; 4]> {
    const MARGIN: i32 = 6;
    let (y0, y1) = (c.top.max(0), (c.top + c.height).min(frame.height));
    if y1 - y0 < 4 {
        return Vec::new();
    }
    let stride = frame.stride as usize;
    let busy = |x: i32| {
        let first = frame.pixels[y0 as usize * stride + x as usize];
        (y0 + 1..y1).step_by(2).any(|y| frame.pixels[y as usize * stride + x as usize] != first)
    };
    let mut spans: Vec<(i32, i32)> = Vec::new();
    let mut x = 0;
    while x < frame.width {
        if !busy(x) {
            x += 1;
            continue;
        }
        let start = x;
        while x < frame.width && busy(x) {
            x += 1;
        }
        let (a, b) = ((start - MARGIN).max(0), (x + MARGIN).min(frame.width));
        match spans.last_mut() {
            // Gaps too narrow to grab are not worth leaving to the system.
            Some(last) if a - last.1 < 2 * MARGIN => last.1 = b,
            _ => spans.push((a, b)),
        }
    }
    spans.into_iter().map(|(a, b)| [a, y0, b - a, y1 - y0]).collect()
}

pub fn render_dirty(st: &mut State) {
    let dirty: Vec<u32> = st.windows.values().filter(|w| w.dirty && w.mapped && w.px.0 > 0).map(|w| w.id).collect();
    for id in dirty {
        render_window(st, id);
    }
}

fn render_window(st: &mut State, id: u32) {
    let Some(w) = st.windows.get(&id) else { return };
    let slot = st.shared.slot(id);
    if !slot.is_attached() {
        return;
    }
    let display = w.display;
    let list = st.paint_list(w);
    // New subsurfaces and popups learn the window's output and scale here.
    for (sid, _, _) in &list {
        crate::output::ensure_scale(st, sid, display);
    }
    let scale = st.window_scale(id);
    let Some(w) = st.windows.get(&id) else { return };
    let (pw, ph) = w.px;
    let caption = w.caption.clone();
    let top = caption.top;
    let mut light = None;
    let mut exclusion = None;
    let drawn = slot.draw(pw, ph, |frame| {
        let mut first = true;
        for (sid, ox, oy) in &list {
            let Some(s) = st.surfaces.get(sid) else { continue };
            let (Some(t), Some((lw, lh)), Some(src)) = (&s.texture, s.logical_size(), s.source_rect()) else { continue };
            // Edges are rounded separately so adjacent surfaces meet exactly.
            let (x, y) = (to_px(*ox as f64, scale), to_px(*oy as f64, scale) + top);
            let (x1, y1) = (to_px((ox + lw) as f64, scale), to_px((oy + lh) as f64, scale) + top);
            if first {
                // Fill only what the toplevel does not cover (live resize).
                let covers = x <= 0 && y <= 0 && x1 >= frame.width && y1 >= frame.height;
                if !(covers && t.opaque) {
                    for row in frame.pixels.chunks_mut(frame.stride as usize) {
                        row.fill(BACKGROUND);
                    }
                }
            }
            blit(frame, t, src, [x, y, x1 - x, y1 - y], first && t.opaque);
            first = false;
        }
        if first {
            for row in frame.pixels.chunks_mut(frame.stride as usize) {
                row.fill(BACKGROUND);
            }
        }
        light = caption_is_light(frame, &caption);
        if caption.auto_exclusion {
            exclusion = Some(header_exclusion(frame, &caption));
        }
    });
    if !drawn {
        return;
    }
    let now = st.now_ms();
    for (sid, _, _) in &list {
        if let Some(s) = st.surfaces.get_mut(sid) {
            for cb in s.frame_cbs.drain(..) {
                cb.done(now);
            }
        }
    }
    let Some(w) = st.windows.get_mut(&id) else { return };
    w.dirty = false;
    if light.is_some() && light != w.caption_light {
        w.caption_light = light;
        st.host.caption_light(id, light.unwrap());
    }
    if let Some(ex) = exclusion.filter(|ex| *ex != w.exclusion) {
        w.exclusion = ex.clone();
        st.host.exclusion(id, &ex);
    }
}
