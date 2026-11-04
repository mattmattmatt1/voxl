//! Screen capture. On Linux the session picks the backend: Wayland (PipeWire,
//! via the desktop portal) when a compositor is up, otherwise X11.

mod wl;
mod x11;

pub use x11::Band;
pub use x11::DAMAGE_EVENTS;

enum Inner {
    X11(x11::Capturer),
    Wl(wl::Capturer),
}

pub struct Capturer {
    pub width: u32,
    pub height: u32,
    pub damaged: bool,
    pub damaged_at: Option<std::time::Instant>,
    pub changes: Vec<(u32, u32, u32, u32)>,
    inner: Inner,
}

impl Capturer {
    pub fn new(display: Option<&str>) -> Result<Self, String> {
        let inner = if crate::backend::get() == crate::backend::Desktop::Wayland {
            Inner::Wl(wl::Capturer::open()?)
        } else {
            Inner::X11(x11::Capturer::new(display)?)
        };
        let (width, height) = match &inner {
            Inner::X11(c) => (c.width, c.height),
            Inner::Wl(c) => (c.width, c.height),
        };
        let mut cap = Capturer {
            width,
            height,
            damaged: false,
            damaged_at: None,
            changes: Vec::new(),
            inner,
        };
        cap.pull();
        Ok(cap)
    }

    fn pull(&mut self) {
        match &mut self.inner {
            Inner::X11(c) => {
                self.damaged = c.damaged;
                self.damaged_at = c.damaged_at;
                self.changes.append(&mut c.changes);
            }
            Inner::Wl(c) => {
                self.damaged = c.damaged;
                self.damaged_at = c.damaged_at;
                self.changes.append(&mut c.changes);
            }
        }
    }

    pub fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        match &self.inner {
            Inner::X11(c) => c.fd(),
            Inner::Wl(c) => c.fd(),
        }
    }

    pub fn pump(&mut self) -> Result<(), String> {
        match &mut self.inner {
            Inner::X11(c) => c.pump()?,
            Inner::Wl(c) => c.pump()?,
        }
        self.pull();
        Ok(())
    }

    pub fn take_damage(&mut self) -> Result<Option<Band>, String> {
        let band = match &mut self.inner {
            Inner::X11(c) => c.take_damage()?,
            Inner::Wl(c) => c.take_damage()?,
        };
        self.damaged = false;
        self.damaged_at = None;
        Ok(band)
    }

    pub fn take_mb_map(&mut self) -> Vec<u8> {
        match &mut self.inner {
            Inner::X11(c) => c.take_mb_map(),
            Inner::Wl(c) => c.take_mb_map(),
        }
    }

    pub fn full(&self) -> Band {
        match &self.inner {
            Inner::X11(c) => c.full(),
            Inner::Wl(c) => c.full(),
        }
    }

    pub fn convert(&self, dst: &mut crate::convert::I420, y0: usize, y1: usize, threads: usize) {
        match &self.inner {
            Inner::X11(c) => c.convert(dst, y0, y1, threads),
            Inner::Wl(c) => c.convert(dst, y0, y1, threads),
        }
    }

    pub fn snapshot(&mut self) -> Result<Vec<u8>, String> {
        let shot = match &mut self.inner {
            Inner::X11(c) => c.snapshot()?,
            Inner::Wl(c) => c.snapshot()?,
        };
        self.pull();
        Ok(shot)
    }

    pub fn bgrx(&self) -> Option<(&[u8], usize)> {
        match &self.inner {
            Inner::X11(c) => c.bgrx(),
            Inner::Wl(c) => c.bgrx(),
        }
    }

    pub fn needs_rebuild(&self) -> bool {
        match &self.inner {
            Inner::X11(c) => c.needs_rebuild(),
            Inner::Wl(c) => c.needs_rebuild(),
        }
    }

    pub fn geometry_changed(&self) -> bool {
        match &self.inner {
            Inner::X11(c) => c.geometry_changed(),
            Inner::Wl(c) => c.geometry_changed(),
        }
    }

    pub fn grab(&mut self, band: Band) -> Result<(), String> {
        match &mut self.inner {
            Inner::X11(c) => c.grab(band),
            Inner::Wl(c) => c.grab(band),
        }
    }
}
