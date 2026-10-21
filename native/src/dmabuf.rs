//! zwp_linux_dmabuf_v1 (version 4): clients hand over GPU buffers as
//! dmabufs. Each one is imported into Vulkan when its wl_buffer is created,
//! so a buffer the GPU can't sample fails right away, not at draw time.
//! Only advertised when the GPU renderer is running.

use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::rc::Rc;
use std::sync::Mutex;

use wayland_protocols::wp::linux_dmabuf::zv1::server::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_feedback_v1::{self, ZwpLinuxDmabufFeedbackV1},
    zwp_linux_dmabuf_v1::{self, ZwpLinuxDmabufV1},
};
use wayland_server::protocol::wl_buffer::WlBuffer;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum};

use crate::state::{Buffer, State, guard};

#[derive(Default)]
pub struct Params {
    planes: [Option<(OwnedFd, u32, u32)>; 4],
    modifier: Option<u64>,
    used: bool,
}

impl GlobalDispatch<ZwpLinuxDmabufV1, ()> for State {
    fn bind(st: &mut State, _: &DisplayHandle, _: &Client, res: New<ZwpLinuxDmabufV1>, _: &(), init: &mut DataInit<'_, State>) {
        let dmabuf = init.init(res, ());
        // Version 4 clients ask for feedback instead.
        if dmabuf.version() >= 4 {
            return;
        }
        let Some(r) = &st.gpu else { return };
        let mut seen = Vec::new();
        for f in &r.gpu.formats {
            if dmabuf.version() >= 3 {
                dmabuf.modifier(f.fourcc, (f.modifier >> 32) as u32, f.modifier as u32);
            } else if !seen.contains(&f.fourcc) {
                seen.push(f.fourcc);
                dmabuf.format(f.fourcc);
            }
        }
    }
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for State {
    fn request(
        st: &mut State,
        client: &Client,
        _: &ZwpLinuxDmabufV1,
        req: zwp_linux_dmabuf_v1::Request,
        _: &(),
        dh: &DisplayHandle,
        init: &mut DataInit<'_, State>,
    ) {
        guard("zwp_linux_dmabuf_v1", client, dh, || match req {
            zwp_linux_dmabuf_v1::Request::CreateParams { params_id } => {
                init.init(params_id, Mutex::new(Params::default()));
            }
            zwp_linux_dmabuf_v1::Request::GetDefaultFeedback { id } => send_feedback(st, &init.init(id, ())),
            zwp_linux_dmabuf_v1::Request::GetSurfaceFeedback { id, .. } => send_feedback(st, &init.init(id, ())),
            _ => {}
        });
    }
}

/// Tranches for the GPU's render node. With direct mode, the first holds
/// what SurfaceFlinger can show as is (no CCS compression), so clients
/// allocate buffers that skip the blit; the second holds the rest.
fn send_feedback(st: &State, fb: &ZwpLinuxDmabufFeedbackV1) {
    let Some(r) = &st.gpu else { return };
    let formats = &r.gpu.formats;
    // The table: { u32 format, u32 padding, u64 modifier } per entry.
    let mut table = Vec::with_capacity(formats.len() * 16);
    for f in formats {
        table.extend(f.fourcc.to_ne_bytes());
        table.extend(0u32.to_ne_bytes());
        table.extend(f.modifier.to_ne_bytes());
    }
    let fd = unsafe { libc::memfd_create(c"awb-dmabuf-formats".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        log::warn!("dmabuf: memfd_create failed");
        return;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut file = std::fs::File::from(fd);
    if std::io::Write::write_all(&mut file, &table).is_err() {
        return;
    }
    let dev = r.gpu.render_node.to_ne_bytes().to_vec();
    fb.format_table(file.as_fd(), table.len() as u32);
    fb.main_device(dev.clone());
    let preferred = |f: &crate::gpu::DmabufFormat| r.direct_enabled() && f.direct();
    let (first, rest): (Vec<u16>, Vec<u16>) = (0..formats.len() as u16).partition(|&i| preferred(&formats[i as usize]));
    for tranche in [first, rest] {
        if tranche.is_empty() {
            continue;
        }
        fb.tranche_target_device(dev.clone());
        fb.tranche_flags(zwp_linux_dmabuf_feedback_v1::TrancheFlags::empty());
        fb.tranche_formats(tranche.iter().flat_map(|i| i.to_ne_bytes()).collect());
        fb.tranche_done();
    }
    fb.done();
}

impl Dispatch<ZwpLinuxDmabufFeedbackV1, ()> for State {
    fn request(
        _: &mut State,
        _: &Client,
        _: &ZwpLinuxDmabufFeedbackV1,
        _: zwp_linux_dmabuf_feedback_v1::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, State>,
    ) {
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, Mutex<Params>> for State {
    fn request(
        st: &mut State,
        client: &Client,
        res: &ZwpLinuxBufferParamsV1,
        req: zwp_linux_buffer_params_v1::Request,
        params: &Mutex<Params>,
        dh: &DisplayHandle,
        init: &mut DataInit<'_, State>,
    ) {
        use zwp_linux_buffer_params_v1::{Error, Request};
        guard("zwp_linux_buffer_params_v1", client, dh, || {
            let mut p = params.lock().unwrap_or_else(|e| e.into_inner());
            match req {
                Request::Add { fd, plane_idx, offset, stride, modifier_hi, modifier_lo } => {
                    let modifier = (modifier_hi as u64) << 32 | modifier_lo as u64;
                    if p.used {
                        res.post_error(Error::AlreadyUsed, "params already used");
                    } else if plane_idx >= 4 {
                        res.post_error(Error::PlaneIdx, "plane index out of range");
                    } else if p.planes[plane_idx as usize].is_some() {
                        res.post_error(Error::PlaneSet, "plane already set");
                    } else if p.modifier.is_some_and(|m| m != modifier) {
                        res.post_error(Error::InvalidFormat, "planes have different modifiers");
                    } else {
                        p.modifier = Some(modifier);
                        p.planes[plane_idx as usize] = Some((fd, offset, stride));
                    }
                }
                Request::Create { width, height, format, flags } => {
                    match create(st, &mut p, res, width, height, format, flags) {
                        Ok(Some(d)) => match client.create_resource::<WlBuffer, (), State>(dh, 1, ()) {
                            Ok(buf) => {
                                st.buffers.insert(buf.id(), Buffer::Dmabuf(Rc::new(d)));
                                res.created(&buf);
                            }
                            Err(_) => res.failed(),
                        },
                        Ok(None) => {}
                        Err(e) => {
                            log::warn!("dmabuf: import failed: {e}");
                            res.failed();
                        }
                    }
                }
                Request::CreateImmed { buffer_id, width, height, format, flags } => {
                    let buf = init.init(buffer_id, ());
                    match create(st, &mut p, res, width, height, format, flags) {
                        Ok(Some(d)) => {
                            st.buffers.insert(buf.id(), Buffer::Dmabuf(Rc::new(d)));
                        }
                        Ok(None) => {}
                        Err(e) => {
                            log::warn!("dmabuf: import failed: {e}");
                            res.post_error(Error::InvalidWlBuffer, format!("cannot import dmabuf: {e}"));
                        }
                    }
                }
                _ => {}
            }
        });
    }
}

/// Validates the params and imports the buffer. Ok(None) means a protocol
/// error was already posted.
fn create(
    st: &State,
    p: &mut Params,
    res: &ZwpLinuxBufferParamsV1,
    width: i32,
    height: i32,
    format: u32,
    flags: WEnum<zwp_linux_buffer_params_v1::Flags>,
) -> Result<Option<crate::gpu::Dmabuf>, String> {
    use zwp_linux_buffer_params_v1::{Error, Flags};
    if p.used {
        res.post_error(Error::AlreadyUsed, "params already used");
        return Ok(None);
    }
    p.used = true;
    let Some(r) = &st.gpu else { return Err("no GPU".into()) };
    let n = p.planes.iter().take_while(|pl| pl.is_some()).count();
    if n == 0 || p.planes[n..].iter().any(|pl| pl.is_some()) {
        res.post_error(Error::Incomplete, "missing planes");
        return Ok(None);
    }
    let modifier = p.modifier.unwrap_or(0);
    let Some(f) = r.gpu.formats.iter().find(|f| f.fourcc == format && f.modifier == modifier) else {
        res.post_error(Error::InvalidFormat, format!("format {format:#x} modifier {modifier:#x} not supported"));
        return Ok(None);
    };
    if f.planes as usize != n {
        res.post_error(Error::Incomplete, format!("modifier needs {} planes, got {n}", f.planes));
        return Ok(None);
    }
    if width <= 0 || height <= 0 || width > 16384 || height > 16384 {
        res.post_error(Error::InvalidDimensions, "invalid size");
        return Ok(None);
    }
    let flags = match flags {
        WEnum::Value(f) => f,
        WEnum::Unknown(_) => return Err("unknown flags".into()),
    };
    if flags.intersects(Flags::Interlaced | Flags::BottomFirst) {
        return Err("interlaced buffers are not supported".into());
    }
    let planes: Vec<_> = p.planes.iter_mut().take(n).map(|pl| pl.take().unwrap()).collect();
    r.gpu.import(planes, width, height, format, modifier, flags.contains(Flags::YInvert)).map(Some)
}
