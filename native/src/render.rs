//! CPU compositing of a window's surfaces into its Android surface.

use crate::Caption;
use crate::android::Frame;
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

/// Draws `t` with its top-left at (x, y) px. `num/den` scales texture px to
/// output px (nearest neighbour; 1/1 is the common case).
fn blit(frame: &mut Frame, t: &Texture, x: i32, y: i32, num: i32, den: i32, opaque_copy: bool) {
    let dw = t.width * num / den;
    let dh = t.height * num / den;
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + dw).min(frame.width);
    let y1 = (y + dh).min(frame.height);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let stride = frame.stride as usize;
    for dy in y0..y1 {
        let sy = ((dy - y) * den / num) as usize;
        let src_row = &t.data[sy * t.width as usize..(sy + 1) * t.width as usize];
        let dst_row = &mut frame.pixels[dy as usize * stride..dy as usize * stride + frame.width as usize];
        if num == den {
            let sx0 = (x0 - x) as usize;
            let src = &src_row[sx0..sx0 + (x1 - x0) as usize];
            let dst = &mut dst_row[x0 as usize..x1 as usize];
            if opaque_copy {
                dst.copy_from_slice(src);
            } else {
                for (d, s) in dst.iter_mut().zip(src) {
                    *d = blend(*s, *d);
                }
            }
        } else {
            for dx in x0..x1 {
                let sx = ((dx - x) * den / num) as usize;
                let s = src_row[sx.min(src_row.len() - 1)];
                let d = &mut dst_row[dx as usize];
                *d = if opaque_copy { s } else { blend(s, *d) };
            }
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
    let scale = st.scale;
    let list = st.paint_list(w);
    let (pw, ph) = w.px;
    let caption = w.caption.clone();
    let top = caption.top;
    let mut light = None;
    let mut exclusion = None;
    let drawn = slot.draw(pw, ph, |frame| {
        let mut first = true;
        for (sid, ox, oy) in &list {
            let Some(s) = st.surfaces.get(sid) else { continue };
            let Some(t) = &s.texture else { continue };
            let bs = s.buffer_scale.max(1);
            let (x, y) = (ox * scale, oy * scale + top);
            if first {
                // Fill only what the toplevel does not cover (live resize).
                let covers = x <= 0 && y <= 0 && x + t.width * scale / bs >= frame.width && y + t.height * scale / bs >= frame.height;
                if !(covers && t.opaque) {
                    for row in frame.pixels.chunks_mut(frame.stride as usize) {
                        row.fill(BACKGROUND);
                    }
                }
            }
            blit(frame, t, x, y, scale, bs, first && t.opaque);
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
