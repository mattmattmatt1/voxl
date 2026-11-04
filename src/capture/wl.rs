//! Wayland frames already converted to tightly packed BGRX by the PipeWire thread.

use super::Band;
use crate::wl;
use nix::errno::Errno;
use nix::unistd::read;
use std::os::fd::{AsFd, BorrowedFd};
use std::time::Instant;

pub struct Capturer {
    shared: std::sync::Arc<wl::Shared>,
    pub width: u32,
    pub height: u32,
    stride: usize,
    buf: Vec<u8>,
    seen: u64,
    pub damaged: bool,
    pub damaged_at: Option<Instant>,
    acc: Option<(u32, u32)>,
    mb_dirty: Vec<u8>,
    mb_spare: Vec<u8>,
    mb_cols: usize,
    pub changes: Vec<(u32, u32, u32, u32)>,
    resize_pending: bool,
    /// Serial whose pixels are currently in `buf`.
    grabbed: u64,
}

impl Capturer {
    pub fn open() -> Result<Self, String> {
        let shared = wl::shared()?;
        let (width, height) = {
            let frame = shared.frame.lock();
            (frame.width, frame.height)
        };
        if width == 0 || height == 0 {
            return Err("wayland stream has no size yet".into());
        }
        let mb_cols = (width as usize).div_ceil(16);
        let mut cap = Capturer {
            shared,
            width,
            height,
            stride: width as usize * 4,
            buf: vec![0; width as usize * height as usize * 4],
            seen: 0,
            damaged: false,
            damaged_at: None,
            acc: None,
            mb_dirty: vec![0; mb_cols * (height as usize).div_ceil(16)],
            mb_spare: Vec::new(),
            mb_cols,
            changes: Vec::new(),
            resize_pending: false,
            grabbed: 0,
        };
        cap.pump()?;
        if cap.seen == 0 {
            return Err("wayland stream produced no frame".into());
        }
        Ok(cap)
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.shared.wake.as_fd()
    }

    pub fn pump(&mut self) -> Result<(), String> {
        let mut scratch = [0u8; 8];
        loop {
            match read(self.shared.wake.as_fd(), &mut scratch) {
                Ok(_) => {}
                Err(Errno::EAGAIN) => break,
                Err(e) => return Err(format!("wayland wake: {e}")),
            }
        }
        let frame = self.shared.frame.lock();
        if frame.serial == self.seen {
            return Ok(());
        }
        if frame.width != self.width || frame.height != self.height {
            self.resize_pending = true;
            return Ok(());
        }
        if frame.bgrx.len() < self.stride * self.height as usize {
            return Ok(());
        }
        let damage = frame.damage.clone();
        self.seen = frame.serial;
        drop(frame);
        if damage.is_empty() {
            self.note(0, 0, self.width, self.height);
        } else {
            for (left, top, right, bot) in damage {
                self.note(left, top, right, bot);
            }
        }
        Ok(())
    }

    fn note(&mut self, left: u32, top: u32, right: u32, bot: u32) {
        if bot <= top || right <= left {
            return;
        }
        let (x0, x1) = (left as usize / 16, (right as usize).div_ceil(16));
        for row in (top as usize / 16)..(bot as usize).div_ceil(16) {
            self.mb_dirty[row * self.mb_cols + x0..row * self.mb_cols + x1].fill(1);
        }
        self.changes.push((left, top, right, bot));
        self.acc = Some(match self.acc {
            Some((a, b)) => (a.min(top), b.max(bot)),
            None => (top, bot),
        });
        self.damaged = true;
        self.damaged_at.get_or_insert_with(Instant::now);
        super::DAMAGE_EVENTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn take_damage(&mut self) -> Result<Option<Band>, String> {
        self.damaged = false;
        self.damaged_at = None;
        let Some((y0, y1)) = self.acc.take() else {
            return Ok(None);
        };
        Ok(Some(Band {
            y0: y0 & !1,
            y1: ((y1 + 1) & !1).min(self.height),
        }))
    }

    pub fn take_bands(&mut self) -> Vec<super::Band> {
        self.damaged = false;
        self.damaged_at = None;
        self.acc = None;
        super::bands_from_mb(&self.mb_dirty, self.mb_cols, self.height)
    }

    pub fn take_mb_map(&mut self) -> Vec<u8> {
        let n = self.mb_dirty.len();
        let mut spare = std::mem::take(&mut self.mb_spare);
        if spare.len() != n {
            spare.resize(n, 0);
        }
        spare.fill(0);
        std::mem::swap(&mut spare, &mut self.mb_dirty);
        spare
    }

    pub fn full(&self) -> Band {
        Band {
            y0: 0,
            y1: self.height,
        }
    }

    pub fn convert(&self, dst: &mut crate::convert::I420, y0: usize, y1: usize, threads: usize) {
        crate::convert::convert_band(&self.buf, self.stride, dst, y0, y1, threads);
    }

    pub fn snapshot(&mut self) -> Result<Vec<u8>, String> {
        self.pump()?;
        if self.resize_pending {
            return Err("wayland size changed during screenshot".into());
        }
        if !self.grab(self.full())? {
            return Err("wayland frame changed during screenshot".into());
        }
        Ok(self.buf.clone())
    }

    pub fn bgrx(&self) -> Option<(&[u8], usize)> {
        Some((&self.buf, self.stride))
    }

    pub fn needs_rebuild(&self) -> bool {
        self.resize_pending
    }

    pub fn geometry_changed(&self) -> bool {
        let frame = self.shared.frame.lock();
        frame.width != self.width || frame.height != self.height
    }

    /// Swap in the frame `pump` observed. A newer serial means those damage rects
    /// belong to a frame that is already gone; the caller keeps the macroblock map
    /// and encodes it against the next frame it does grab.
    /// `band` selects rows for the X11 capturer; Wayland publishes a whole frame.
    pub fn grab(&mut self, _band: Band) -> Result<bool, String> {
        let mut frame = self.shared.frame.lock();
        if frame.width != self.width || frame.height != self.height {
            self.resize_pending = true;
            return Ok(false);
        }
        if frame.serial != self.seen {
            // pump already recorded this serial's rects. Keep the capture
            // loop awake so the next pass encodes them against this frame.
            self.damaged = true;
            self.damaged_at.get_or_insert_with(Instant::now);
            return Ok(false);
        }
        if self.grabbed == self.seen {
            return Ok(true);
        }
        let n = self.stride * self.height as usize;
        if frame.bgrx.len() < n || self.buf.len() < n {
            return Ok(false);
        }
        std::mem::swap(&mut frame.bgrx, &mut self.buf);
        self.grabbed = self.seen;
        Ok(true)
    }
}
