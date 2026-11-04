//! PipeWire consumer for the portal's screen-cast node. Frames are stored as
//! tightly packed BGRX so the rest of voxl can treat them like an X image.

use super::Shared;
use pipewire as pw;
use pw::spa::param::video::VideoFormat;
use pw::{properties::properties, spa};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Unknown,
    Bgrx,
    Rgbx,
    Rgb,
    Bgr,
    I420,
    Nv12,
}

struct UserData {
    shared: Arc<Shared>,
    format: spa::param::video::VideoInfoRaw,
    kind: Kind,
    width: u32,
    height: u32,
    logged: bool,
}

static DMA_LOGGED: AtomicBool = AtomicBool::new(false);

pub fn spawn(shared: Arc<Shared>, fd: OwnedFd, node: u32) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("pipewire".into())
        .spawn(move || {
            if let Err(e) = run(shared, fd, node, &tx) {
                let _ = tx.send(Err(e));
            }
        })
        .map_err(|e| e.to_string())?;
    match rx.recv_timeout(Duration::from_secs(15)) {
        Ok(r) => r,
        Err(_) => Err("PipeWire stream did not start".into()),
    }
}

fn run(
    shared: Arc<Shared>,
    fd: OwnedFd,
    node: u32,
    tx: &std::sync::mpsc::SyncSender<Result<(), String>>,
) -> Result<(), String> {
    let _guard = pw::init();
    let mainloop = pw::main_loop::MainLoop::new(None).map_err(|e| e.to_string())?;
    let context = pw::context::Context::new(&mainloop).map_err(|e| e.to_string())?;
    let core = context.connect_fd(fd, None).map_err(|e| e.to_string())?;
    let data = UserData {
        shared,
        format: Default::default(),
        kind: Kind::Unknown,
        width: 0,
        height: 0,
        logged: false,
    };
    let stream = pw::stream::Stream::new(
        &core,
        "voxl",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| e.to_string())?;
    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_, _, old, new| {
            log::debug!("pipewire stream {old:?} -> {new:?}");
        })
        .param_changed(|_, user_data, id, param| {
            let Some(param) = param else {
                return;
            };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let (media_type, media_subtype) =
                match pw::spa::param::format_utils::parse_format(param) {
                    Ok(v) => v,
                    Err(_) => return,
                };
            if media_type != pw::spa::param::format::MediaType::Video
                || media_subtype != pw::spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            if user_data.format.parse(param).is_err() {
                return;
            }
            let info = &user_data.format;
            user_data.width = info.size().width;
            user_data.height = info.size().height;
            user_data.kind = kind_of(info.format().as_raw());
            if !user_data.logged {
                user_data.logged = true;
                log::info!(
                    "pipewire format {:?} {}x{}",
                    info.format(),
                    user_data.width,
                    user_data.height
                );
            }
        })
        .process(|stream, user_data| {
            if user_data.kind == Kind::Unknown || user_data.width == 0 || user_data.height == 0 {
                return;
            }
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &datas[0];
            let Some((src, stride)) = plane(data) else {
                if !DMA_LOGGED.swap(true, Ordering::Relaxed) {
                    log::warn!("pipewire buffer is not mappable; want a memfd screen cast");
                }
                return;
            };
            let mut frame = user_data.shared.frame.lock();
            if !pack(
                &src,
                stride,
                user_data.width,
                user_data.height,
                user_data.kind,
                &mut frame.bgrx,
            ) {
                return;
            }
            frame.width = user_data.width;
            frame.height = user_data.height;
            frame.serial = frame.serial.wrapping_add(1).max(1);
            let raw = user_data.shared.wake.as_raw_fd();
            drop(frame);
            poke(raw);
        })
        .register()
        .map_err(|e| e.to_string())?;

    let obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
            VideoFormat::RGB,
            VideoFormat::BGR,
            VideoFormat::I420,
            VideoFormat::NV12,
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle {
                width: 1280,
                height: 720
            },
            spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            spa::utils::Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 60, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 240, denom: 1 }
        ),
    );
    let values: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map_err(|e| format!("pipewire format pod: {e}"))?
    .0
    .into_inner();
    let mut params = [spa::pod::Pod::from_bytes(&values).ok_or("pipewire format pod is empty")?];
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| e.to_string())?;
    log::info!("pipewire capture node {node}");
    let _ = tx.send(Ok(()));
    mainloop.run();
    Ok(())
}

fn kind_of(raw: u32) -> Kind {
    let is = |fmt: VideoFormat| fmt.as_raw() == raw;
    if is(VideoFormat::BGRx) || is(VideoFormat::BGRA) {
        Kind::Bgrx
    } else if is(VideoFormat::RGBx) || is(VideoFormat::RGBA) {
        Kind::Rgbx
    } else if is(VideoFormat::RGB) {
        Kind::Rgb
    } else if is(VideoFormat::BGR) {
        Kind::Bgr
    } else if is(VideoFormat::I420) {
        Kind::I420
    } else if is(VideoFormat::NV12) {
        Kind::Nv12
    } else {
        Kind::Unknown
    }
}

/// Bytes of one video plane.
///
/// The `data` pointer is sometimes an address from the producer, or a
/// `PROT_NONE` reservation. `mincore` still reports those pages as mapped, and
/// a normal copy segfaults. `process_vm_readv` returns `EFAULT` instead.
/// A memfd on `spa_data.fd` is the mapping that belongs to this process.
fn plane(data: &spa::buffer::Data) -> Option<(Vec<u8>, i32)> {
    let raw = data.as_raw();
    let maxsize = raw.maxsize as usize;
    // The format range allows 8192×8192. A padded 4K plane can be larger than
    // 32 MiB, so the ceiling is the format maximum, and the copy is still the
    // chunk's own byte count.
    const MAX_PLANE: usize = 8192 * 8192 * 4;
    if maxsize == 0 {
        return None;
    }
    let chunk = data.chunk();
    let stride = chunk.stride();
    let offset = (chunk.offset() as usize) % maxsize;
    let size = (chunk.size() as usize).min(maxsize - offset);
    if size == 0 || size > MAX_PLANE {
        return None;
    }
    let ptr = raw.data as *const u8;
    if !ptr.is_null() {
        if let Some(bytes) = copy_bytes(unsafe { ptr.add(offset) }, size) {
            return Some((bytes, stride));
        }
    }
    if raw.fd < 0 {
        return None;
    }
    // Map the bytes this chunk uses, not the whole maxsize reservation.
    let map_len = (raw.mapoffset as usize)
        .saturating_add(offset)
        .saturating_add(size);
    if map_len == 0 || raw.mapoffset as usize > MAX_PLANE {
        return None;
    }
    let mapped = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            map_len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            raw.fd as libc::c_int,
            0,
        )
    };
    if mapped == libc::MAP_FAILED {
        return None;
    }
    let start = (raw.mapoffset as usize).saturating_add(offset);
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let file_end = if unsafe { libc::fstat(raw.fd as libc::c_int, &mut st) } == 0 && st.st_size > 0
    {
        st.st_size as usize
    } else {
        map_len
    };
    let n = size.min(file_end.saturating_sub(start).min(map_len.saturating_sub(start)));
    let copy = if n > 0 {
        copy_bytes(unsafe { (mapped as *const u8).add(start) }, n)
    } else {
        None
    };
    unsafe { libc::munmap(mapped, map_len) };
    copy.map(|bytes| (bytes, stride))
}

fn copy_bytes(ptr: *const u8, len: usize) -> Option<Vec<u8>> {
    if ptr.is_null() || len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len];
    let local = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: len,
    };
    let remote = libc::iovec {
        iov_base: ptr as *mut libc::c_void,
        iov_len: len,
    };
    let n = unsafe { libc::process_vm_readv(libc::getpid(), &local, 1, &remote, 1, 0) };
    if n == len as isize {
        Some(buf)
    } else {
        None
    }
}

fn poke(fd: i32) {
    let one = 1u64;
    unsafe {
        libc::write(fd, &one as *const u64 as *const libc::c_void, 8);
    }
}

fn pack(src: &[u8], stride_in: i32, width: u32, height: u32, kind: Kind, dst: &mut Vec<u8>) -> bool {
    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 {
        return false;
    }
    dst.resize(w * h * 4, 0);
    let bpp = match kind {
        Kind::Rgb | Kind::Bgr => 3,
        Kind::I420 | Kind::Nv12 => 1,
        _ => 4,
    };
    let stride = if stride_in > 0 {
        stride_in as usize
    } else {
        w * bpp
    };
    match kind {
        Kind::Bgrx => copy_swap(src, stride, w, h, dst, false),
        Kind::Rgbx => copy_swap(src, stride, w, h, dst, true),
        Kind::Rgb => copy3(src, stride, w, h, dst, true),
        Kind::Bgr => copy3(src, stride, w, h, dst, false),
        Kind::I420 => i420_to_bgrx(src, stride, w, h, dst),
        Kind::Nv12 => nv12_to_bgrx(src, stride, w, h, dst),
        Kind::Unknown => false,
    }
}

fn copy_swap(src: &[u8], stride: usize, w: usize, h: usize, dst: &mut [u8], swap: bool) -> bool {
    if stride < w * 4 || src.len() < stride * h {
        return false;
    }
    for row in 0..h {
        let s = &src[row * stride..][..w * 4];
        let d = &mut dst[row * w * 4..][..w * 4];
        if !swap {
            d.copy_from_slice(s);
        } else {
            for (px, out) in s.chunks_exact(4).zip(d.chunks_exact_mut(4)) {
                out[0] = px[2];
                out[1] = px[1];
                out[2] = px[0];
                out[3] = 255;
            }
        }
    }
    true
}

fn copy3(src: &[u8], stride: usize, w: usize, h: usize, dst: &mut [u8], rgb: bool) -> bool {
    if stride < w * 3 || src.len() < stride * h {
        return false;
    }
    for row in 0..h {
        let s = &src[row * stride..];
        let d = &mut dst[row * w * 4..][..w * 4];
        for x in 0..w {
            let p = &s[x * 3..x * 3 + 3];
            if rgb {
                d[x * 4] = p[2];
                d[x * 4 + 1] = p[1];
                d[x * 4 + 2] = p[0];
            } else {
                d[x * 4] = p[0];
                d[x * 4 + 1] = p[1];
                d[x * 4 + 2] = p[2];
            }
            d[x * 4 + 3] = 255;
        }
    }
    true
}

fn i420_to_bgrx(src: &[u8], y_stride: usize, w: usize, h: usize, dst: &mut [u8]) -> bool {
    let c_stride = y_stride.div_ceil(2);
    let y_size = y_stride * h;
    let c_size = c_stride * h.div_ceil(2);
    if src.len() < y_size + c_size * 2 {
        return false;
    }
    let (y, rest) = src.split_at(y_size);
    let (u, v) = rest.split_at(c_size);
    yuv_to_bgrx(y, y_stride, u, c_stride, v, c_stride, false, w, h, dst);
    true
}

fn nv12_to_bgrx(src: &[u8], y_stride: usize, w: usize, h: usize, dst: &mut [u8]) -> bool {
    let y_size = y_stride * h;
    let c_stride = y_stride;
    let c_size = c_stride * h.div_ceil(2);
    if src.len() < y_size + c_size {
        return false;
    }
    let (y, uv) = src.split_at(y_size);
    yuv_to_bgrx(y, y_stride, uv, c_stride, uv, c_stride, true, w, h, dst);
    true
}

fn yuv_to_bgrx(
    y: &[u8],
    y_stride: usize,
    u: &[u8],
    u_stride: usize,
    v: &[u8],
    v_stride: usize,
    nv12: bool,
    w: usize,
    h: usize,
    dst: &mut [u8],
) {
    for row in 0..h {
        let ys = &y[row * y_stride..];
        let crow = row / 2;
        let us = &u[crow * u_stride..];
        let vs = &v[crow * v_stride..];
        let d = &mut dst[row * w * 4..][..w * 4];
        for x in 0..w {
            let (cb, cr) = if nv12 {
                let o = x & !1;
                (us.get(o).copied().unwrap_or(128), us.get(o + 1).copied().unwrap_or(128))
            } else {
                (
                    us.get(x / 2).copied().unwrap_or(128),
                    vs.get(x / 2).copied().unwrap_or(128),
                )
            };
            let (b, g, r) = yuv601(ys.get(x).copied().unwrap_or(0), cb, cr);
            d[x * 4] = b;
            d[x * 4 + 1] = g;
            d[x * 4 + 2] = r;
            d[x * 4 + 3] = 255;
        }
    }
}

fn yuv601(y: u8, u: u8, v: u8) -> (u8, u8, u8) {
    let c = y as i32 - 16;
    let d = u as i32 - 128;
    let e = v as i32 - 128;
    let r = (298 * c + 409 * e + 128) >> 8;
    let g = (298 * c - 100 * d - 208 * e + 128) >> 8;
    let b = (298 * c + 516 * d + 128) >> 8;
    (
        b.clamp(0, 255) as u8,
        g.clamp(0, 255) as u8,
        r.clamp(0, 255) as u8,
    )
}
