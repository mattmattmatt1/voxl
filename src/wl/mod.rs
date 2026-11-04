//! Wayland desktop: xdg-desktop-portal screen cast (PipeWire) plus remote-desktop input.
//!
//! This is the default on a session that has `WAYLAND_DISPLAY` set. X11 remains the
//! path for Xvfb and for `--backend=x11`.

mod pw;

use crate::desktop::ClipboardMode;
use crate::hub::{Hub, Out};
use crate::input::InputEvent;
use ashpd::desktop::remote_desktop::{
    DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions,
};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::PersistMode;
use bytes::Bytes;
use nix::sys::eventfd::{EfdFlags, EventFd};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::os::fd::OwnedFd;
use std::sync::{mpsc, OnceLock};
use std::time::{Duration, Instant};

pub struct Frame {
    pub bgrx: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub serial: u64,
}

pub struct Shared {
    pub frame: Mutex<Frame>,
    pub wake: OwnedFd,
}

static SHARED: OnceLock<std::sync::Arc<Shared>> = OnceLock::new();
static INPUT: OnceLock<tokio::sync::mpsc::UnboundedSender<Cmd>> = OnceLock::new();

pub fn shared() -> Result<std::sync::Arc<Shared>, String> {
    SHARED
        .get()
        .cloned()
        .ok_or_else(|| "wayland capture is not running".into())
}

enum Cmd {
    Move(f64, f64),
    Button { button: i32, down: bool },
    /// Smooth scroll in stream pixels. `finish` closes the gesture.
    Axis { dx: f64, dy: f64, finish: bool },
    /// Wheel notches. Positive is down or right.
    AxisDiscrete { dx: i32, dy: i32 },
    Key { keysym: i32, down: bool },
    Release,
}

struct Handoff {
    fd: OwnedFd,
    node: u32,
}

/// Opens the portal, starts the PipeWire capture thread, and returns once a frame exists.
pub async fn start() -> Result<(), String> {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let portal_task = tokio::spawn(async move {
        if let Err(e) = portal(Some(ready_tx), cmd_rx).await {
            log::error!("wayland portal: {e}");
        }
    });
    let fail = |portal_task: tokio::task::JoinHandle<()>, err: String| async move {
        // Close the remote-desktop session before the caller falls back to X11.
        portal_task.abort();
        let _ = portal_task.await;
        err
    };
    let handoff = match ready_rx.await {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => return Err(fail(portal_task, e).await),
        Err(_) => {
            return Err(
                fail(
                    portal_task,
                    "wayland portal stopped before sharing the screen".into(),
                )
                .await,
            );
        }
    };

    let wake = match EventFd::from_flags(EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK) {
        Ok(fd) => fd.into(),
        Err(e) => return Err(fail(portal_task, e.to_string()).await),
    };
    let shared = std::sync::Arc::new(Shared {
        frame: Mutex::new(Frame {
            bgrx: Vec::new(),
            width: 0,
            height: 0,
            serial: 0,
        }),
        wake,
    });
    if let Err(e) = pw::spawn(shared.clone(), handoff.fd, handoff.node) {
        return Err(fail(portal_task, e).await);
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if shared.frame.lock().serial > 0 {
            break;
        }
        if Instant::now() >= deadline {
            return Err(fail(
                portal_task,
                "the screen cast produced no frames (share permission still pending, or the compositor sent nothing)".into(),
            )
            .await);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (w, h) = {
        let frame = shared.frame.lock();
        (frame.width, frame.height)
    };
    log::info!("wayland capture {w}x{h}");
    let _ = SHARED.set(shared);
    let _ = INPUT.set(cmd_tx);
    Ok(())
}

async fn portal(
    mut ready: Option<tokio::sync::oneshot::Sender<Result<Handoff, String>>>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Cmd>,
) -> Result<(), String> {
    match try_remote(&mut ready, &mut rx).await {
        Ok(true) => return Ok(()),
        Ok(false) => log::warn!("remote-desktop portal has no screen stream; trying screen cast only"),
        Err(e) => log::warn!("remote-desktop portal unavailable ({e}); trying screen cast only"),
    }
    if ready.is_none() {
        return Ok(());
    }
    match try_cast(&mut ready, &mut rx).await {
        Ok(()) => Ok(()),
        Err(e) => {
            if let Some(tx) = ready.take() {
                let _ = tx.send(Err(e.clone()));
            }
            Err(e)
        }
    }
}

fn sources() -> SelectSourcesOptions {
    SelectSourcesOptions::default()
        .set_cursor_mode(CursorMode::Hidden)
        .set_sources(ashpd::enumflags2::BitFlags::from(SourceType::Monitor))
        .set_multiple(false)
        .set_persist_mode(PersistMode::DoNot)
}

async fn try_remote(
    ready: &mut Option<tokio::sync::oneshot::Sender<Result<Handoff, String>>>,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Cmd>,
) -> Result<bool, String> {
    let rd = RemoteDesktop::new().await.map_err(|e| e.to_string())?;
    let session = rd
        .create_session(Default::default())
        .await
        .map_err(|e| e.to_string())?;
    rd.select_devices(
        &session,
        SelectDevicesOptions::default()
            .set_devices(DeviceType::Keyboard | DeviceType::Pointer),
    )
    .await
    .map_err(|e| e.to_string())?;
    let sc = Screencast::new().await.map_err(|e| e.to_string())?;
    sc.select_sources(&session, sources())
        .await
        .map_err(|e| e.to_string())?;
    let response = rd
        .start(&session, None, Default::default())
        .await
        .map_err(|e| e.to_string())?
        .response()
        .map_err(|e| e.to_string())?;
    let Some(stream) = response.streams().first().cloned() else {
        return Ok(false);
    };
    let node = stream.pipe_wire_node_id();
    let fd = sc
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .map_err(|e| e.to_string())?;
    log::info!(
        "wayland remote desktop, pipewire node {node}, devices {:?}",
        response.devices()
    );
    let logical = stream_logical(&stream);
    if let Some(tx) = ready.take() {
        let _ = tx.send(Ok(Handoff { fd, node }));
    }
    drive(&rd, &session, node, logical, rx).await;
    Ok(true)
}

async fn try_cast(
    ready: &mut Option<tokio::sync::oneshot::Sender<Result<Handoff, String>>>,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Cmd>,
) -> Result<(), String> {
    let sc = Screencast::new().await.map_err(|e| e.to_string())?;
    let session = sc
        .create_session(Default::default())
        .await
        .map_err(|e| e.to_string())?;
    sc.select_sources(&session, sources())
        .await
        .map_err(|e| e.to_string())?;
    let response = sc
        .start(&session, None, Default::default())
        .await
        .map_err(|e| e.to_string())?
        .response()
        .map_err(|e| e.to_string())?;
    let stream = response
        .streams()
        .first()
        .cloned()
        .ok_or("the screen cast portal returned no stream")?;
    let node = stream.pipe_wire_node_id();
    let fd = sc
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .map_err(|e| e.to_string())?;
    log::info!("wayland screen cast only (no input), pipewire node {node}");
    if let Some(tx) = ready.take() {
        let _ = tx.send(Ok(Handoff { fd, node }));
    }
    let mut warned = false;
    while let Some(cmd) = rx.recv().await {
        if !warned && !matches!(cmd, Cmd::Release) {
            log::warn!("this compositor's portal shared the screen but not the keyboard or pointer");
            warned = true;
        }
    }
    drop(session);
    Ok(())
}

fn stream_logical(stream: &ashpd::desktop::screencast::Stream) -> Option<(f64, f64)> {
    stream.size().and_then(|(w, h)| {
        if w > 0 && h > 0 {
            Some((w as f64, h as f64))
        } else {
            None
        }
    })
}

/// Portal pointer coordinates are the stream's logical size, which is not the
/// PipeWire buffer size when the output is scaled.
fn map_point(x: f64, y: f64, frame: (f64, f64), logical: Option<(f64, f64)>) -> (f64, f64) {
    let Some((lw, lh)) = logical else {
        return (x, y);
    };
    let (fw, fh) = frame;
    if fw <= 0.0 || fh <= 0.0 || lw <= 0.0 || lh <= 0.0 {
        return (x, y);
    }
    (
        (x * lw / fw).clamp(0.0, (lw - 1.0).max(0.0)),
        (y * lh / fh).clamp(0.0, (lh - 1.0).max(0.0)),
    )
}

fn map_delta(dx: f64, dy: f64, frame: (f64, f64), logical: Option<(f64, f64)>) -> (f64, f64) {
    let Some((lw, lh)) = logical else {
        return (dx, dy);
    };
    let (fw, fh) = frame;
    if fw <= 0.0 || fh <= 0.0 || lw <= 0.0 || lh <= 0.0 {
        return (dx, dy);
    }
    (dx * lw / fw, dy * lh / fh)
}

fn frame_size() -> (f64, f64) {
    SHARED
        .get()
        .map(|shared| {
            let frame = shared.frame.lock();
            (frame.width as f64, frame.height as f64)
        })
        .unwrap_or((0.0, 0.0))
}

async fn drive(
    rd: &RemoteDesktop,
    session: &ashpd::desktop::Session<RemoteDesktop>,
    node: u32,
    logical: Option<(f64, f64)>,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Cmd>,
) {
    let mut keys = HashSet::new();
    let mut buttons = HashSet::new();
    let mut warned = false;
    while let Some(cmd) = rx.recv().await {
        if let Err(e) = apply(rd, session, node, logical, cmd, &mut keys, &mut buttons).await {
            if !warned {
                log::warn!("wayland input: {e}");
                warned = true;
            }
        }
    }
}

async fn apply(
    rd: &RemoteDesktop,
    session: &ashpd::desktop::Session<RemoteDesktop>,
    node: u32,
    logical: Option<(f64, f64)>,
    cmd: Cmd,
    keys: &mut HashSet<i32>,
    buttons: &mut HashSet<i32>,
) -> Result<(), String> {
    match cmd {
        Cmd::Move(x, y) => {
            let (x, y) = map_point(x, y, frame_size(), logical);
            rd.notify_pointer_motion_absolute(session, node, x, y, Default::default())
                .await
                .map_err(|e| e.to_string())
        }
        Cmd::Button { button, down } => {
            if down {
                if !buttons.insert(button) {
                    return Ok(());
                }
            } else if !buttons.remove(&button) {
                return Ok(());
            }
            let state = if down { KeyState::Pressed } else { KeyState::Released };
            rd.notify_pointer_button(session, button, state, Default::default())
                .await
                .map_err(|e| e.to_string())
        }
        Cmd::Axis { dx, dy, finish } => {
            // NotifyPointerAxis is in the same pixels as pointer motion, not notches.
            // finish on every sample clears Mutter's detent accumulator.
            let (dx, dy) = map_delta(dx, dy, frame_size(), logical);
            rd.notify_pointer_axis(
                session,
                dx,
                dy,
                ashpd::desktop::remote_desktop::NotifyPointerAxisOptions::default()
                    .set_finish(finish),
            )
            .await
            .map_err(|e| e.to_string())
        }
        Cmd::AxisDiscrete { dx, dy } => {
            use ashpd::desktop::remote_desktop::Axis;
            if dy != 0 {
                rd.notify_pointer_axis_discrete(
                    session,
                    Axis::Vertical,
                    dy,
                    Default::default(),
                )
                .await
                .map_err(|e| e.to_string())?;
            }
            if dx != 0 {
                rd.notify_pointer_axis_discrete(
                    session,
                    Axis::Horizontal,
                    dx,
                    Default::default(),
                )
                .await
                .map_err(|e| e.to_string())?;
            }
            Ok(())
        }
        Cmd::Key { keysym, down } => {
            if down {
                keys.insert(keysym);
            } else if !keys.remove(&keysym) {
                return Ok(());
            }
            let state = if down { KeyState::Pressed } else { KeyState::Released };
            rd.notify_keyboard_keysym(session, keysym, state, Default::default())
                .await
                .map_err(|e| e.to_string())
        }
        Cmd::Release => {
            for key in keys.drain() {
                let _ = rd
                    .notify_keyboard_keysym(session, key, KeyState::Released, Default::default())
                    .await;
            }
            for button in buttons.drain() {
                let _ = rd
                    .notify_pointer_button(session, button, KeyState::Released, Default::default())
                    .await;
            }
            Ok(())
        }
    }
}

pub fn spawn_input(
    mode: ClipboardMode,
    px_per_notch: f64,
) -> Result<mpsc::Sender<InputEvent>, String> {
    let portal = INPUT
        .get()
        .cloned()
        .ok_or("wayland input is not running")?;
    let (tx, rx) = mpsc::channel::<InputEvent>();
    // Wayland scroll is already in desktop pixels. The notch size is the X11 path.
    let _ = px_per_notch;
    std::thread::Builder::new()
        .name("input".into())
        .spawn(move || input_loop(rx, portal, mode))
        .map_err(|e| e.to_string())?;
    Ok(tx)
}

fn input_loop(
    rx: mpsc::Receiver<InputEvent>,
    portal: tokio::sync::mpsc::UnboundedSender<Cmd>,
    mode: ClipboardMode,
) {
    let send = |cmd: Cmd| {
        let _ = portal.send(cmd);
    };
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        while let Ok(ev) = rx.try_recv() {
            batch.push(ev);
        }
        let mut last_move = None;
        for ev in batch {
            match ev {
                InputEvent::Move { x, y } => last_move = Some((x, y)),
                other => {
                    if let Some((x, y)) = last_move.take() {
                        send(Cmd::Move(x as f64, y as f64));
                    }
                    match other {
                        InputEvent::Button { button, down } => {
                            if let Some(button) = linux_button(button) {
                                send(Cmd::Button { button, down });
                            }
                        }
                        InputEvent::Scroll { dx, dy } => {
                            send(Cmd::AxisDiscrete { dx, dy });
                        }
                        InputEvent::ScrollPx { dx, dy } => {
                            if dx.abs() >= 0.5 || dy.abs() >= 0.5 {
                                send(Cmd::Axis {
                                    dx,
                                    dy,
                                    finish: false,
                                });
                            }
                        }
                        InputEvent::Key { keysym, down } => {
                            send(Cmd::Key {
                                keysym: keysym as i32,
                                down,
                            });
                        }
                        InputEvent::Clipboard(crate::desktop::ClipContent::Text(text)) => {
                            if mode.to_server {
                                set_clipboard(&text);
                            }
                        }
                        InputEvent::Clipboard(crate::desktop::ClipContent::Image(_)) => {
                            log::debug!("wayland clipboard images are not forwarded");
                        }
                        InputEvent::ReleaseAll | InputEvent::Shutdown => send(Cmd::Release),
                        InputEvent::Move { .. } => {}
                    }
                }
            }
        }
        if let Some((x, y)) = last_move {
            send(Cmd::Move(x as f64, y as f64));
        }
    }
}

/// Linux evdev button codes (`input-event-codes.h`), which is what the portal wants.
fn linux_button(button: u8) -> Option<i32> {
    Some(match button {
        1 => 0x110, // BTN_LEFT
        2 => 0x112, // BTN_MIDDLE
        3 => 0x111, // BTN_RIGHT
        8 => 0x113, // BTN_SIDE
        9 => 0x114, // BTN_EXTRA
        _ => return None,
    })
}

pub fn spawn_desktop(hub: std::sync::Arc<Hub>, mode: ClipboardMode) {
    send_arrow(&hub);
    if !mode.to_client {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("clipboard".into())
        .spawn(move || {
            loop {
                if let Some(text) = read_clipboard() {
                    publish(&hub, &text);
                }
                std::thread::sleep(Duration::from_millis(400));
            }
        });
}

fn publish(hub: &Hub, text: &str) {
    if text.is_empty() {
        return;
    }
    {
        let mut last = hub.last_clipboard.lock();
        if last.as_deref() == Some(text) {
            return;
        }
        *last = Some(text.to_string());
    }
    let msg = serde_json::json!({"t": "clip", "d": text}).to_string();
    hub.broadcast(|| Out::Text(msg.clone()));
}

fn read_clipboard() -> Option<String> {
    wl_paste().or_else(klipper_get)
}

fn wl_paste() -> Option<String> {
    let out = std::process::Command::new("timeout")
        .args(["0.4", "wl-paste", "-n", "-t", "text/plain"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let text = text.trim_end_matches(['\n', '\r']).to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn klipper_get() -> Option<String> {
    let out = std::process::Command::new("dbus-send")
        .args([
            "--session",
            "--print-reply",
            "--dest=org.kde.klipper",
            "/klipper",
            "org.kde.klipper.klipper.getClipboardContents",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_dbus_string(&String::from_utf8_lossy(&out.stdout))
}

fn set_clipboard(text: &str) -> bool {
    if text.contains('\0') {
        return false;
    }
    if wl_copy(text) {
        return true;
    }
    klipper_set(text)
}

fn wl_copy(text: &str) -> bool {
    let mut child = match std::process::Command::new("wl-copy")
        .stdin(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let Some(mut stdin) = child.stdin.take() else {
        return false;
    };
    use std::io::Write;
    if stdin.write_all(text.as_bytes()).is_err() {
        return false;
    }
    drop(stdin);
    child.wait().map(|s| s.success()).unwrap_or(false)
}

fn klipper_set(text: &str) -> bool {
    std::process::Command::new("dbus-send")
        .args([
            "--session",
            "--dest=org.kde.klipper",
            "/klipper",
            "org.kde.klipper.klipper.setClipboardContents",
            &format!("string:{text}"),
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn parse_dbus_string(reply: &str) -> Option<String> {
    let line = reply.lines().find(|l| l.contains("string"))?;
    let start = line.find('"')? + 1;
    let end = line.rfind('"')?;
    if end <= start {
        return None;
    }
    Some(line[start..end].to_string())
}

fn send_arrow(hub: &Hub) {
    // 16x16 left pointer, white with a black edge. Hotspot is the tip.
    const ROWS: [u16; 16] = [
        0b1100_0000_0000_0000,
        0b1110_0000_0000_0000,
        0b1111_0000_0000_0000,
        0b1111_1000_0000_0000,
        0b1111_1100_0000_0000,
        0b1111_1110_0000_0000,
        0b1111_1111_0000_0000,
        0b1111_1111_1000_0000,
        0b1111_1111_1100_0000,
        0b1111_1110_0000_0000,
        0b1110_1110_0000_0000,
        0b1100_0111_0000_0000,
        0b1000_0111_0000_0000,
        0b0000_0011_1000_0000,
        0b0000_0011_1000_0000,
        0b0000_0001_0000_0000,
    ];
    let (w, h) = (16u16, 16u16);
    let mut msg = Vec::with_capacity(14 + 16 * 16 * 4);
    msg.push(0x03);
    msg.push(0);
    msg.extend_from_slice(&w.to_be_bytes());
    msg.extend_from_slice(&h.to_be_bytes());
    msg.extend_from_slice(&1u16.to_be_bytes());
    msg.extend_from_slice(&1u16.to_be_bytes());
    msg.extend_from_slice(&1u32.to_be_bytes());
    for (y, row) in ROWS.iter().enumerate() {
        for x in 0..16 {
            let on = row & (1 << (15 - x)) != 0;
            let edge = on
                && (x == 0
                    || y == 0
                    || x == 15
                    || y == 15
                    || ROWS[y] & (1 << (15 - (x - 1).min(15))) == 0
                    || ROWS[y.saturating_sub(1)] & (1 << (15 - x)) == 0
                    || ROWS[(y + 1).min(15)] & (1 << (15 - x)) == 0);
            if on && edge {
                msg.extend_from_slice(&[0, 0, 0, 255]);
            } else if on {
                msg.extend_from_slice(&[255, 255, 255, 255]);
            } else {
                msg.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    let b = Bytes::from(msg);
    *hub.cursor.lock() = Some(b.clone());
    hub.broadcast(|| Out::Bin(b.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_uses_the_stream_logical_size() {
        let frame = (2560.0, 1440.0);
        let logical = Some((1280.0, 720.0));
        assert_eq!(map_point(0.0, 0.0, frame, logical), (0.0, 0.0));
        assert_eq!(map_point(2560.0, 1440.0, frame, logical), (1279.0, 719.0));
        assert_eq!(map_point(100.0, 40.0, frame, None), (100.0, 40.0));
        assert_eq!(map_delta(120.0, -40.0, frame, logical), (60.0, -20.0));
        assert_eq!(map_delta(10.0, 10.0, (1280.0, 720.0), logical), (10.0, 10.0));
    }
}
