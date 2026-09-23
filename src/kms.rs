//! Temporarily takes over the display via DRM/KMS to show the transition
//! from our own double-buffered framebuffers, with vblank-synced page flips.
//! fbcon's framebuffer is never touched.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::time::{Duration, Instant};

use drm::buffer::DrmFourcc;
use drm::control::dumbbuffer::DumbBuffer;
use drm::control::{connector, crtc, framebuffer, Device as ControlDevice, Event, Mode, PageFlipFlags};
use drm::Device;

use crate::snapshot::{PixFmt, Snapshot};

struct Card(File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl Device for Card {}
impl ControlDevice for Card {}

struct Buffer {
    dumb: DumbBuffer,
    fb: framebuffer::Handle,
    ptr: *mut u8,
    len: usize,
    pitch: usize,
}

pub struct Kms {
    card: Card,
    crtc: crtc::Handle,
    conns: Vec<connector::Handle>,
    mode: Mode,
    pub width: u32,
    pub height: u32,
    bufs: Vec<Buffer>,
    back: usize,
}

// Raw mapping pointers are only touched from the single worker thread.
unsafe impl Send for Kms {}

fn active_output(card: &Card) -> io::Result<(crtc::Handle, Vec<connector::Handle>, Mode)> {
    let res = card.resource_handles()?;
    let mut found: Option<(crtc::Handle, Mode)> = None;
    let mut conns = Vec::new();
    for &c in res.connectors() {
        let info = card.get_connector(c, false)?;
        if info.state() != connector::State::Connected {
            continue;
        }
        let Some(enc) = info.current_encoder() else { continue };
        let Some(crtc) = card.get_encoder(enc)?.crtc() else { continue };
        match found {
            None => {
                let ci = card.get_crtc(crtc)?;
                let (Some(mode), Some(_)) = (ci.mode(), ci.framebuffer()) else { continue };
                found = Some((crtc, mode));
                conns.push(c);
            }
            Some((f, _)) if f == crtc => conns.push(c),
            _ => {}
        }
    }
    let (crtc, mode) = found.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no active CRTC"))?;
    Ok((crtc, conns, mode))
}

impl Kms {
    /// Opens the first DRM card driving an active display. Does not take
    /// over anything yet — see [`Kms::acquire_master`] and [`Kms::present`].
    pub fn open(fmt: PixFmt) -> io::Result<Kms> {
        let mut last_err = io::Error::new(io::ErrorKind::NotFound, "no DRM card with an active display");
        for n in 0..8 {
            let path = format!("/dev/dri/card{n}");
            let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) else { continue };
            let card = Card(file);
            match active_output(&card) {
                Ok((crtc, conns, mode)) => return Kms::setup(card, crtc, conns, mode, fmt),
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }

    fn setup(card: Card, crtc: crtc::Handle, conns: Vec<connector::Handle>, mode: Mode, fmt: PixFmt) -> io::Result<Kms> {
        let (w, h) = mode.size();
        let (fourcc, depth, bpp) = match fmt {
            PixFmt::Xrgb8888 => (DrmFourcc::Xrgb8888, 24, 32),
            PixFmt::Rgb565 => (DrmFourcc::Rgb565, 16, 16),
        };
        let mut kms = Kms {
            card,
            crtc,
            conns,
            mode,
            width: w as u32,
            height: h as u32,
            bufs: Vec::new(),
            back: 0,
        };
        for _ in 0..2 {
            let mut dumb = kms.card.create_dumb_buffer((w as u32, h as u32), fourcc, bpp)?;
            let fb = match kms.card.add_framebuffer(&dumb, depth, bpp) {
                Ok(fb) => fb,
                Err(e) => {
                    let _ = kms.card.destroy_dumb_buffer(dumb);
                    return Err(e);
                }
            };
            let pitch = drm::buffer::Buffer::pitch(&dumb) as usize;
            let mut map = kms.card.map_dumb_buffer(&mut dumb)?;
            let (ptr, len) = (map.as_mut_ptr(), map.len());
            // Kept mapped for the buffer's lifetime; unmapped in Drop.
            std::mem::forget(map);
            kms.bufs.push(Buffer { dumb, fb, ptr, len, pitch });
        }
        Ok(kms)
    }

    pub fn acquire_master(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.card.acquire_master_lock().is_ok() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    pub fn drop_master(&self) {
        let _ = self.card.release_master_lock();
    }

    /// True while one of our framebuffers is what the CRTC is scanning out.
    pub fn is_showing_ours(&self) -> bool {
        match self.card.get_crtc(self.crtc).ok().and_then(|c| c.framebuffer()) {
            Some(fb) => self.bufs.iter().any(|b| b.fb == fb),
            None => false,
        }
    }

    /// Polls until someone else (fbcon, X) has put their own framebuffer back.
    pub fn wait_released(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !self.is_showing_ours() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        !self.is_showing_ours()
    }

    /// Hands out the back buffer's mapped memory and pitch for drawing.
    pub fn back_buffer(&mut self) -> (&mut [u8], usize) {
        let b = &self.bufs[self.back];
        (unsafe { std::slice::from_raw_parts_mut(b.ptr, b.len) }, b.pitch)
    }

    pub fn draw_snapshot(&mut self, img: &Snapshot) {
        let rb = img.row_bytes();
        let h = img.height as usize;
        let (mem, pitch) = self.back_buffer();
        for y in 0..h {
            mem[y * pitch..y * pitch + rb].copy_from_slice(img.row(y));
        }
    }

    /// Displays the back buffer at the next vblank and waits for it.
    pub fn present(&mut self) -> io::Result<()> {
        let fb = self.bufs[self.back].fb;
        let flipped = self
            .card
            .page_flip(self.crtc, fb, PageFlipFlags::EVENT, None)
            .and_then(|_| self.wait_flip());
        if flipped.is_err() {
            // Page flips can't change pixel format and aren't supported by
            // every driver; a legacy SETCRTC with the same mode still works.
            self.card.set_crtc(self.crtc, Some(fb), (0, 0), &self.conns, Some(self.mode))?;
        }
        self.back ^= 1;
        Ok(())
    }

    fn wait_flip(&self) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_millis(200);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "page flip timed out"));
            }
            let mut pfd = libc::pollfd { fd: self.card.0.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            let n = unsafe { libc::poll(&mut pfd, 1, remaining.as_millis() as i32) };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if n == 0 {
                continue;
            }
            for ev in self.card.receive_events()? {
                if let Event::PageFlip(_) = ev {
                    return Ok(());
                }
            }
        }
    }
}

impl Drop for Kms {
    fn drop(&mut self) {
        for b in self.bufs.drain(..) {
            unsafe { libc::munmap(b.ptr as *mut _, b.len) };
            let _ = self.card.destroy_framebuffer(b.fb);
            let _ = self.card.destroy_dumb_buffer(b.dumb);
        }
    }
}
