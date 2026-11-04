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
    mb_cols: usize,
    pub changes: Vec<(u32, u32, u32, u32)>,
    resize_pending: bool,
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
            mb_cols,
            changes: Vec::new(),
            resize_pending: false,
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
        let n = self.stride * self.height as usize;
        if frame.bgrx.len() < n {
            return Ok(());
        }
        self.buf[..n].copy_from_slice(&frame.bgrx[..n]);
        self.seen = frame.serial;
        drop(frame);
        self.note(0, 0, self.width, self.height);
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

    pub fn take_mb_map(&mut self) -> Vec<u8> {
        let n = self.mb_dirty.len();
        std::mem::replace(&mut self.mb_dirty, vec![0; n])
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
        self.grab(self.full())?;
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

    /// The PipeWire thread already stored a full BGRX frame. Copy the band in case a
    /// newer one landed after `pump`.
    pub fn grab(&mut self, band: Band) -> Result<(), String> {
        let frame = self.shared.frame.lock();
        if frame.width != self.width || frame.height != self.height {
            self.resize_pending = true;
            return Ok(());
        }
        let y0 = band.y0.min(self.height) as usize;
        let y1 = band.y1.min(self.height) as usize;
        let row = self.stride;
        let need = y1.saturating_mul(row);
        if frame.bgrx.len() < need || self.buf.len() < need {
            return Ok(());
        }
        for y in y0..y1 {
            let off = y * row;
            self.buf[off..off + row].copy_from_slice(&frame.bgrx[off..off + row]);
        }
        self.seen = frame.serial;
        Ok(())
    }
}
