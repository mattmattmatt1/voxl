//! Screen capture. On Linux the session picks the backend: Wayland (PipeWire,
//! via the desktop portal) when a compositor is up, otherwise X11.

mod wl;
mod x11;

pub use x11::Band;
pub use x11::DAMAGE_EVENTS;

/// Dirty macroblock rows as even-aligned y spans. A clean row between two dirty
/// rows stays a gap, so a clock and a window do not repaint the desktop between them.
pub fn bands_from_mb(mb: &[u8], cols: usize, height: u32) -> Vec<Band> {
    if cols == 0 || mb.is_empty() {
        return Vec::new();
    }
    let rows = mb.len() / cols;
    let mut out = Vec::new();
    let mut start: Option<u32> = None;
    let push = |out: &mut Vec<Band>, y0: u32, y1: u32| {
        let y0 = y0 & !1;
        let y1 = ((y1 + 1) & !1).min(height);
        if y1 > y0 {
            out.push(Band { y0, y1 });
        }
    };
    for row in 0..rows {
        let dirty = mb[row * cols..(row + 1) * cols].iter().any(|&v| v != 0);
        if dirty {
            start.get_or_insert((row as u32) * 16);
        } else if let Some(y0) = start.take() {
            push(&mut out, y0, (row as u32) * 16);
        }
    }
    if let Some(y0) = start {
        push(&mut out, y0, height);
    }
    out
}

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

    #[allow(dead_code)] // macOS and Windows still take one span; Linux uses take_bands.
    pub fn take_damage(&mut self) -> Result<Option<Band>, String> {
        let band = match &mut self.inner {
            Inner::X11(c) => c.take_damage()?,
            Inner::Wl(c) => c.take_damage()?,
        };
        self.damaged = false;
        self.damaged_at = None;
        Ok(band)
    }

    /// Every dirty y span since the last take. Does not clear the macroblock map;
    /// [`Self::take_mb_map`] does that.
    pub fn take_bands(&mut self) -> Result<Vec<Band>, String> {
        let bands = match &mut self.inner {
            Inner::X11(c) => c.take_bands(),
            Inner::Wl(c) => c.take_bands(),
        };
        self.damaged = false;
        self.damaged_at = None;
        Ok(bands)
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

    /// `false` means the grabbed pixels do not match the damage just taken (a newer
    /// Wayland frame landed). The dirty macroblock map is left in place so the next
    /// grab encodes those rects from the newest pixels.
    pub fn grab(&mut self, band: Band) -> Result<bool, String> {
        match &mut self.inner {
            Inner::X11(c) => {
                c.grab(band)?;
                Ok(true)
            }
            Inner::Wl(c) => c.grab(band),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::bands_from_mb;

    #[test]
    fn bands_keep_a_clean_gap() {
        let cols = 2;
        let mut mb = vec![0u8; cols * 4];
        mb[0] = 1;
        mb[cols * 3] = 1;
        let bands = bands_from_mb(&mb, cols, 64);
        assert_eq!(bands.len(), 2);
        assert_eq!((bands[0].y0, bands[0].y1), (0, 16));
        assert_eq!((bands[1].y0, bands[1].y1), (48, 64));
    }
}
