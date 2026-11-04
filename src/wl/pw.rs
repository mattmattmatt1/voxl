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
    /// Packed BGRX being filled from the current PipeWire buffer.
    staging: Vec<u8>,
    /// Last frame that was published. Idle frames are detected against this.
    prev: Vec<u8>,
    /// Scratch for a plane that still has to be packed into `staging`.
    raw: Vec<u8>,
}

static DMA_LOGGED: AtomicBool = AtomicBool::new(false);

pub fn spawn(shared: Arc<Shared>, fd: OwnedFd, node: u32, max_fps: u32) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("pipewire".into())
        .spawn(move || {
            if let Err(e) = run(shared, fd, node, max_fps, &tx) {
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
    max_fps: u32,
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
        staging: Vec::new(),
        prev: Vec::new(),
        raw: Vec::new(),
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
            let raw = unsafe { stream.dequeue_raw_buffer() };
            if raw.is_null() {
                return;
            }
            let recycle = Recycle { stream, buf: raw };
            let spa_buf = unsafe { (*raw).buffer };
            if spa_buf.is_null() || unsafe { (*spa_buf).n_datas } == 0 || unsafe { (*spa_buf).datas.is_null() }
            {
                return;
            }
            if !load_bgrx(
                spa_buf,
                user_data.width,
                user_data.height,
                user_data.kind,
                &mut user_data.staging,
                &mut user_data.raw,
            ) {
                if !DMA_LOGGED.swap(true, Ordering::Relaxed) {
                    log::warn!("pipewire buffer is not mappable; want a memfd screen cast");
                }
                return;
            }
            let damage = match video_damage(spa_buf, user_data.width, user_data.height) {
                Some(rects) if !full_damage(&rects, user_data.width, user_data.height) => rects,
                _ => diff_mbs(
                    &user_data.staging,
                    &user_data.prev,
                    user_data.width,
                    user_data.height,
                ),
            };
            if damage.is_empty() {
                return;
            }
            let wake = user_data.shared.wake.as_raw_fd();
            {
                let mut frame = user_data.shared.frame.lock();
                if frame.bgrx.len() != user_data.staging.len() {
                    frame.bgrx.resize(user_data.staging.len(), 0);
                }
                frame.bgrx.copy_from_slice(&user_data.staging);
                frame.damage.clone_from(&damage);
                frame.width = user_data.width;
                frame.height = user_data.height;
                frame.serial = frame.serial.wrapping_add(1).max(1);
            }
            std::mem::swap(&mut user_data.staging, &mut user_data.prev);
            drop(recycle);
            poke(wake);
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
            spa::utils::Fraction {
                num: max_fps.max(1),
                denom: 1,
            }
        ),
    );
    let meta = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamMeta.as_raw(),
        id: spa::param::ParamType::Meta.as_raw(),
        properties: vec![
            pod_id(pw::spa::sys::SPA_PARAM_META_type, pw::spa::sys::SPA_META_VideoDamage),
            pod_int(
                pw::spa::sys::SPA_PARAM_META_size,
                (std::mem::size_of::<pw::spa::sys::spa_meta_region>() * 16) as i32,
            ),
        ],
    };
    let memfd = 1i32 << pw::spa::sys::SPA_DATA_MemFd;
    let memptr = 1i32 << pw::spa::sys::SPA_DATA_MemPtr;
    let buffers = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![pod_flags(
            pw::spa::sys::SPA_PARAM_BUFFERS_dataType,
            memfd | memptr,
            &[memfd, memptr],
        )],
    };
    let values = pod_bytes(&spa::pod::Value::Object(obj))?;
    let meta_bytes = pod_bytes(&spa::pod::Value::Object(meta))?;
    let buffer_bytes = pod_bytes(&spa::pod::Value::Object(buffers))?;
    let mut params = [
        spa::pod::Pod::from_bytes(&values).ok_or("pipewire format pod is empty")?,
        spa::pod::Pod::from_bytes(&meta_bytes).ok_or("pipewire meta pod is empty")?,
        spa::pod::Pod::from_bytes(&buffer_bytes).ok_or("pipewire buffer pod is empty")?,
    ];
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

/// Returns the PipeWire buffer to the stream when the process callback is done,
/// including every early return. The buffer must be queued before the capturer is woken.
struct Recycle<'a> {
    stream: &'a pw::stream::StreamRef,
    buf: *mut pw::sys::pw_buffer,
}

impl Drop for Recycle<'_> {
    fn drop(&mut self) {
        unsafe { self.stream.queue_raw_buffer(self.buf) };
    }
}

/// The format range allows 8192×8192. A padded 4K plane can be larger than 32 MiB.
const MAX_PLANE: usize = 8192 * 8192 * 4;

extern "C" {
    fn I420ToARGB(
        src_y: *const u8,
        src_stride_y: i32,
        src_u: *const u8,
        src_stride_u: i32,
        src_v: *const u8,
        src_stride_v: i32,
        dst_argb: *mut u8,
        dst_stride_argb: i32,
        width: i32,
        height: i32,
    ) -> i32;
    fn NV12ToARGB(
        src_y: *const u8,
        src_stride_y: i32,
        src_uv: *const u8,
        src_stride_uv: i32,
        dst_argb: *mut u8,
        dst_stride_argb: i32,
        width: i32,
        height: i32,
    ) -> i32;
}

/// One safe read of a PipeWire chunk into `dst`.
///
/// `spa_data.data` is sometimes a producer address or a `PROT_NONE` reservation.
/// Copying it with `memcpy` segfaults. `process_vm_readv` returns `EFAULT` instead.
/// A memfd is mapped `PROT_READ` in this process and only that mapping is copied.
fn read_chunk(data: &spa::buffer::Data, dst: &mut Vec<u8>) -> Option<(i32, usize)> {
    let raw = data.as_raw();
    let maxsize = raw.maxsize as usize;
    if maxsize == 0 || maxsize > MAX_PLANE {
        return None;
    }
    let chunk = data.chunk();
    let stride = chunk.stride();
    let offset = chunk.offset() as usize;
    if offset >= maxsize {
        return None;
    }
    let size = (chunk.size() as usize).min(maxsize - offset);
    if size == 0 || size > MAX_PLANE {
        return None;
    }
    if dst.len() < size {
        dst.resize(size, 0);
    }
    let ptr = raw.data as *const u8;
    if !ptr.is_null() && read_remote(unsafe { ptr.add(offset) }, &mut dst[..size]) {
        return Some((stride, size));
    }
    if raw.fd < 0 {
        return None;
    }
    let map_len = (raw.mapoffset as usize)
        .saturating_add(offset)
        .saturating_add(size);
    if map_len == 0 || map_len > MAX_PLANE {
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
    let ok = n > 0
        && unsafe {
            let src = std::slice::from_raw_parts((mapped as *const u8).add(start), n);
            dst[..n].copy_from_slice(src);
            true
        };
    unsafe { libc::munmap(mapped, map_len) };
    ok.then_some((stride, n))
}

fn read_remote(ptr: *const u8, dst: &mut [u8]) -> bool {
    if ptr.is_null() || dst.is_empty() {
        return false;
    }
    let local = libc::iovec {
        iov_base: dst.as_mut_ptr().cast(),
        iov_len: dst.len(),
    };
    let remote = libc::iovec {
        iov_base: ptr as *mut libc::c_void,
        iov_len: dst.len(),
    };
    let n = unsafe { libc::process_vm_readv(libc::getpid(), &local, 1, &remote, 1, 0) };
    n == dst.len() as isize
}

fn data_at(buf: *mut pw::spa::sys::spa_buffer, index: u32) -> Option<*const spa::buffer::Data> {
    unsafe {
        if buf.is_null() || (*buf).datas.is_null() || index >= (*buf).n_datas {
            return None;
        }
        Some((*buf).datas.add(index as usize) as *const spa::buffer::Data)
    }
}

/// Fill `staging` with tightly packed BGRX. `raw` is reused for planes that are not already BGRX.
fn load_bgrx(
    buf: *mut pw::spa::sys::spa_buffer,
    width: u32,
    height: u32,
    kind: Kind,
    staging: &mut Vec<u8>,
    raw: &mut Vec<u8>,
) -> bool {
    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 {
        return false;
    }
    let n = w * h * 4;
    if staging.len() != n {
        staging.resize(n, 0);
    }
    let Some(first_ptr) = data_at(buf, 0) else {
        return false;
    };
    let first = unsafe { &*first_ptr };
    match kind {
        Kind::Bgrx => {
            let row = w * 4;
            let Some((stride, size)) = read_chunk(first, staging) else {
                return false;
            };
            // Tightly packed BGRx is already the frame. A padded stride is packed below.
            if stride == row as i32 && size >= n {
                staging.truncate(n);
                return true;
            }
            raw.clear();
            raw.extend_from_slice(&staging[..size]);
            if staging.len() != n {
                staging.resize(n, 0);
            }
            pack(&raw[..size], stride, width, height, Kind::Bgrx, staging)
        }
        Kind::Rgbx | Kind::Rgb | Kind::Bgr => {
            let Some((stride, size)) = read_chunk(first, raw) else {
                return false;
            };
            pack(&raw[..size], stride, width, height, kind, staging)
        }
        Kind::I420 => load_i420(buf, width, height, staging, raw),
        Kind::Nv12 => load_nv12(buf, width, height, staging, raw),
        Kind::Unknown => false,
    }
}

fn load_i420(
    buf: *mut pw::spa::sys::spa_buffer,
    width: u32,
    height: u32,
    staging: &mut [u8],
    raw: &mut Vec<u8>,
) -> bool {
    let w = width as i32;
    let h = height as i32;
    let planes = unsafe { (*buf).n_datas };
    if planes >= 3 {
        let mut planes_buf = [Vec::new(), Vec::new(), Vec::new()];
        let mut strides = [0i32; 3];
        for i in 0..3 {
            let Some(data) = data_at(buf, i) else {
                return false;
            };
            let Some((stride, size)) = read_chunk(unsafe { &*data }, &mut planes_buf[i as usize]) else {
                return false;
            };
            if size == 0 {
                return false;
            }
            strides[i as usize] = if stride > 0 {
                stride
            } else {
                return false;
            };
        }
        return unsafe {
            I420ToARGB(
                planes_buf[0].as_ptr(),
                strides[0],
                planes_buf[1].as_ptr(),
                strides[1],
                planes_buf[2].as_ptr(),
                strides[2],
                staging.as_mut_ptr(),
                width as i32 * 4,
                w,
                h,
            ) == 0
        };
    }
    let Some((stride, size)) = read_chunk(unsafe { &*data_at(buf, 0).unwrap() }, raw) else {
        return false;
    };
    let y_stride = if stride > 0 { stride as usize } else { width as usize };
    let y_size = y_stride * height as usize;
    let c_stride = y_stride.div_ceil(2);
    let c_size = c_stride * (height as usize).div_ceil(2);
    if size < y_size + c_size * 2 {
        return false;
    }
    let y = raw.as_ptr();
    let u = unsafe { y.add(y_size) };
    let v = unsafe { u.add(c_size) };
    unsafe {
        I420ToARGB(
            y,
            y_stride as i32,
            u,
            c_stride as i32,
            v,
            c_stride as i32,
            staging.as_mut_ptr(),
            width as i32 * 4,
            w,
            h,
        ) == 0
    }
}

fn load_nv12(
    buf: *mut pw::spa::sys::spa_buffer,
    width: u32,
    height: u32,
    staging: &mut [u8],
    raw: &mut Vec<u8>,
) -> bool {
    let w = width as i32;
    let h = height as i32;
    if unsafe { (*buf).n_datas } >= 2 {
        let mut y_buf = Vec::new();
        let mut uv_buf = Vec::new();
        let Some(ydata) = data_at(buf, 0) else {
            return false;
        };
        let Some(uvdata) = data_at(buf, 1) else {
            return false;
        };
        let Some((ys, _)) = read_chunk(unsafe { &*ydata }, &mut y_buf) else {
            return false;
        };
        let Some((cs, _)) = read_chunk(unsafe { &*uvdata }, &mut uv_buf) else {
            return false;
        };
        if ys <= 0 || cs <= 0 {
            return false;
        }
        return unsafe {
            NV12ToARGB(
                y_buf.as_ptr(),
                ys,
                uv_buf.as_ptr(),
                cs,
                staging.as_mut_ptr(),
                width as i32 * 4,
                w,
                h,
            ) == 0
        };
    }
    let Some((stride, size)) = read_chunk(unsafe { &*data_at(buf, 0).unwrap() }, raw) else {
        return false;
    };
    let y_stride = if stride > 0 { stride as usize } else { width as usize };
    let y_size = y_stride * height as usize;
    let c_size = y_stride * (height as usize).div_ceil(2);
    if size < y_size + c_size {
        return false;
    }
    unsafe {
        NV12ToARGB(
            raw.as_ptr(),
            y_stride as i32,
            raw.as_ptr().add(y_size),
            y_stride as i32,
            staging.as_mut_ptr(),
            width as i32 * 4,
            w,
            h,
        ) == 0
    }
}

/// Rectangles are `(left, top, right, bottom)`. An empty list means nothing changed.
/// A missing previous frame is the whole screen: a wrong baseline must not look idle.
fn diff_mbs(cur: &[u8], prev: &[u8], width: u32, height: u32) -> Vec<(u32, u32, u32, u32)> {
    let w = width as usize;
    let h = height as usize;
    let n = w * h * 4;
    if n == 0 {
        return Vec::new();
    }
    if cur.len() < n || prev.len() != n {
        return vec![(0, 0, width, height)];
    }
    let cols = w.div_ceil(16);
    let rows = h.div_ceil(16);
    let mut rects = Vec::new();
    let mut dirty = 0usize;
    for my in 0..rows {
        let y0 = my * 16;
        let y1 = (y0 + 16).min(h);
        for mx in 0..cols {
            let x0 = mx * 16;
            let x1 = (x0 + 16).min(w);
            let mut same = true;
            for y in y0..y1 {
                let off = y * w * 4 + x0 * 4;
                let len = (x1 - x0) * 4;
                if cur[off..off + len] != prev[off..off + len] {
                    same = false;
                    break;
                }
            }
            if !same {
                dirty += 1;
                rects.push((x0 as u32, y0 as u32, x1 as u32, y1 as u32));
            }
        }
    }
    if dirty == 0 {
        Vec::new()
    } else if dirty * 2 > cols * rows {
        vec![(0, 0, width, height)]
    } else {
        rects
    }
}

fn full_damage(rects: &[(u32, u32, u32, u32)], width: u32, height: u32) -> bool {
    rects.len() == 1 && rects[0].0 == 0 && rects[0].1 == 0 && rects[0].2 >= width && rects[0].3 >= height
}

/// `SPA_META_VideoDamage` is an array of `spa_meta_region`, ended by a zero-size entry.
/// Missing or empty meta is `None`: the caller compares pixels. An empty list must not
/// mean "nothing changed", because a compositor that omits the meta still has a new frame.
fn video_damage(
    buf: *mut pw::spa::sys::spa_buffer,
    width: u32,
    height: u32,
) -> Option<Vec<(u32, u32, u32, u32)>> {
    let meta = unsafe { pw::spa::sys::spa_buffer_find_meta(buf, pw::spa::sys::SPA_META_VideoDamage) };
    if meta.is_null() {
        return None;
    }
    let meta = unsafe { &*meta };
    let region_size = std::mem::size_of::<pw::spa::sys::spa_meta_region>();
    if meta.data.is_null() || region_size == 0 || (meta.size as usize) < region_size {
        return None;
    }
    let n = (meta.size as usize) / region_size;
    let ptr = meta.data as *const pw::spa::sys::spa_meta_region;
    let mut rects = Vec::new();
    for i in 0..n.min(64) {
        let region = unsafe { (*ptr.add(i)).region };
        let rw = region.size.width;
        let rh = region.size.height;
        if rw == 0 || rh == 0 {
            break;
        }
        let x = region.position.x;
        let y = region.position.y;
        let left = x.max(0) as u32;
        let top = y.max(0) as u32;
        if left >= width || top >= height {
            continue;
        }
        let right = ((x as i64) + rw as i64).clamp(0, width as i64) as u32;
        let bot = ((y as i64) + rh as i64).clamp(0, height as i64) as u32;
        if right > left && bot > top {
            rects.push((left, top, right, bot));
        }
    }
    if rects.is_empty() {
        None
    } else {
        Some(rects)
    }
}

fn pod_bytes(value: &spa::pod::Value) -> Result<Vec<u8>, String> {
    let (cur, _) = spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), value)
        .map_err(|e| format!("pipewire pod: {e:?}"))?;
    Ok(cur.into_inner())
}

fn pod_id(key: u32, id: u32) -> spa::pod::Property {
    spa::pod::Property::new(key, spa::pod::Value::Id(spa::utils::Id(id)))
}

fn pod_int(key: u32, value: i32) -> spa::pod::Property {
    spa::pod::Property::new(key, spa::pod::Value::Int(value))
}

fn pod_flags(key: u32, default: i32, flags: &[i32]) -> spa::pod::Property {
    spa::pod::Property::new(
        key,
        spa::pod::Value::Choice(spa::pod::ChoiceValue::Int(spa::utils::Choice(
            spa::utils::ChoiceFlags::empty(),
            spa::utils::ChoiceEnum::Flags {
                default,
                flags: flags.to_vec(),
            },
        ))),
    )
}

fn poke(fd: i32) {
    let one = 1u64;
    unsafe {
        libc::write(fd, &one as *const u64 as *const libc::c_void, 8);
    }
}

fn pack(src: &[u8], stride_in: i32, width: u32, height: u32, kind: Kind, dst: &mut [u8]) -> bool {
    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 || dst.len() < w * h * 4 {
        return false;
    }
    let bpp = match kind {
        Kind::Rgb | Kind::Bgr => 3,
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
        Kind::I420 | Kind::Nv12 | Kind::Unknown => false,
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

#[cfg(test)]
mod tests {
    use super::diff_mbs;

    fn frame(w: u32, h: u32, fill: u8) -> Vec<u8> {
        vec![fill; w as usize * h as usize * 4]
    }

    #[test]
    fn diff_identical_is_empty() {
        let a = frame(32, 32, 1);
        assert!(diff_mbs(&a, &a, 32, 32).is_empty());
    }

    #[test]
    fn diff_one_block() {
        let mut cur = frame(32, 32, 0);
        let prev = frame(32, 32, 0);
        cur[0] = 9;
        assert_eq!(diff_mbs(&cur, &prev, 32, 32), vec![(0, 0, 16, 16)]);
    }

    #[test]
    fn diff_missing_baseline_is_the_whole_frame() {
        let cur = frame(32, 16, 0);
        assert_eq!(diff_mbs(&cur, &[], 32, 16), vec![(0, 0, 32, 16)]);
    }

    #[test]
    fn diff_mostly_changed_is_one_rect() {
        let cur = frame(32, 32, 0);
        let prev = frame(32, 32, 1);
        assert_eq!(diff_mbs(&cur, &prev, 32, 32), vec![(0, 0, 32, 32)]);
    }
}
