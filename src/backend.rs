//! Which Linux desktop voxl captures.
//!
//! A Wayland session is the default. X11 is the default only when there is no
//! Wayland compositor (Xvfb, a classic X session, Linux Mint's X11 session),
//! or when `--backend=x11` / `VOXL_BACKEND=x11` says so.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Desktop {
    X11,
    Wayland,
}

const UNSET: u8 = 0;
const X11: u8 = 1;
const WAYLAND: u8 = 2;

static KIND: AtomicU8 = AtomicU8::new(UNSET);
static EXPLICIT: AtomicBool = AtomicBool::new(false);

/// Reads `--backend` (or `VOXL_BACKEND`) and the session, and remembers the choice.
pub fn init(flag: Option<&str>) -> Desktop {
    let from_env = std::env::var("VOXL_BACKEND").ok();
    let flag = flag.map(str::trim).filter(|s| !s.is_empty());
    let (desktop, explicit) = decide(flag, from_env.as_deref(), session_is_wayland());
    KIND.store(if desktop == Desktop::Wayland { WAYLAND } else { X11 }, Ordering::Relaxed);
    EXPLICIT.store(explicit, Ordering::Relaxed);
    desktop
}

pub fn get() -> Desktop {
    match KIND.load(Ordering::Relaxed) {
        WAYLAND => Desktop::Wayland,
        X11 => Desktop::X11,
        _ => {
            if session_is_wayland() {
                Desktop::Wayland
            } else {
                Desktop::X11
            }
        }
    }
}

pub fn explicit() -> bool {
    EXPLICIT.load(Ordering::Relaxed)
}

/// Wayland failed and the user did not insist on it. Later openers use X11.
pub fn force_x11() {
    KIND.store(X11, Ordering::Relaxed);
}

pub fn session_is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE")
            .ok()
            .is_some_and(|s| s.eq_ignore_ascii_case("wayland"))
}

fn decide(flag: Option<&str>, env: Option<&str>, wayland_session: bool) -> (Desktop, bool) {
    let raw = flag.or(env).map(|s| s.to_ascii_lowercase());
    match raw.as_deref() {
        Some("wayland") => (Desktop::Wayland, true),
        Some("x11") => (Desktop::X11, true),
        Some(other) => {
            log::warn!("unknown --backend={other}; choosing from the session");
            (if wayland_session { Desktop::Wayland } else { Desktop::X11 }, false)
        }
        None => (if wayland_session { Desktop::Wayland } else { Desktop::X11 }, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wayland_session_is_the_default() {
        assert_eq!(decide(None, None, true), (Desktop::Wayland, false));
        assert_eq!(decide(None, None, false), (Desktop::X11, false));
    }

    #[test]
    fn explicit_backend_wins() {
        assert_eq!(decide(Some("x11"), None, true), (Desktop::X11, true));
        assert_eq!(decide(None, Some("wayland"), false), (Desktop::Wayland, true));
        assert_eq!(decide(Some("X11"), Some("wayland"), true), (Desktop::X11, true));
    }
}
