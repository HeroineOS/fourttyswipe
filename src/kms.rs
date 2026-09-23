//! Temporarily takes over the display via DRM/KMS to show the transition
//! from our own double-buffered framebuffers, with vblank-synced page flips.
//! fbcon's and the GUI sessions' framebuffers are never written to.
//!
//! The card is opened once and kept open. Opening a DRM primary node makes
//! the opener master whenever nobody currently is — which is exactly the
//! state a GUI session is in for a moment right after its VT is activated,
//! before it re-takes master. Re-opening per transition could steal master
//! in that window and leave the GUI unable to show anything.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::time::{Duration, Instant};

use drm::buffer::{DrmFourcc, DrmModifier};
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

struct Output {
    crtc: crtc::Handle,
    conns: Vec<connector::Handle>,
    mode: Mode,
}

pub struct Kms {
    card: Card,
    output: Option<Output>,
    bufs: Vec<Buffer>,
    buf_fmt: Option<PixFmt>,
    back: usize,
}

// Raw mapping pointers are only touched from the single worker thread.
unsafe impl Send for Kms {}

fn other(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

fn active_output(card: &Card) -> io::Result<Output> {
    let res = card.resource_handles()?;
    let mut found: Option<Output> = None;
    for &c in res.connectors() {
        let info = card.get_connector(c, false)?;
        if info.state() != connector::State::Connected {
            continue;
        }
        let Some(enc) = info.current_encoder() else { continue };
        let Some(crtc) = card.get_encoder(enc)?.crtc() else { continue };
        match &mut found {
            None => {
                let ci = card.get_crtc(crtc)?;
                let (Some(mode), Some(_)) = (ci.mode(), ci.framebuffer()) else { continue };
                found = Some(Output { crtc, conns: vec![c], mode });
            }
            Some(o) if o.crtc == crtc => o.conns.push(c),
            _ => {}
        }
    }
    found.ok_or_else(|| other("no active display output"))
}

impl Kms {
    /// Opens the DRM card that drives the display and makes sure we don't
    /// hold master afterwards.
    pub fn open() -> io::Result<Kms> {
        let mut last_err = other("no DRM card with display connectors");
        for n in 0..8 {
            let Ok(file) = OpenOptions::new().read(true).write(true).open(format!("/dev/dri/card{n}")) else {
                continue;
            };
            let card = Card(file);
            let _ = card.release_master_lock();
            match card.resource_handles() {
                Ok(res) if !res.connectors().is_empty() => {
                    return Ok(Kms { card, output: None, bufs: Vec::new(), buf_fmt: None, back: 0 });
                }
                Ok(_) => {}
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }

    /// Re-reads which CRTC/mode is currently lighting the screen (the
    /// console and a GUI session can use different ones).
    pub fn refresh_output(&mut self) -> io::Result<()> {
        self.output = Some(active_output(&self.card)?);
        Ok(())
    }

    pub fn size(&self) -> Option<(u32, u32)> {
        self.output.as_ref().map(|o| {
            let (w, h) = o.mode.size();
            (w as u32, h as u32)
        })
    }

    pub fn refresh_hz(&self) -> f64 {
        self.output.as_ref().map(|o| o.mode.vrefresh()).filter(|&r| r > 0).unwrap_or(60) as f64
    }

    /// Allocates (and pre-faults) the two framebuffers for the current
    /// output, reusing existing ones when size and format still match.
    pub fn ensure_buffers(&mut self, fmt: PixFmt) -> io::Result<()> {
        let (w, h) = self.size().ok_or_else(|| other("no output"))?;
        if self.buf_fmt == Some(fmt)
            && self.bufs.first().map(|b| drm::buffer::Buffer::size(&b.dumb)) == Some((w, h))
        {
            return Ok(());
        }
        if !self.release_buffers() {
            return Err(other("old buffers still on screen"));
        }
        let (fourcc, depth, bpp) = match fmt {
            PixFmt::Xrgb8888 => (DrmFourcc::Xrgb8888, 24, 32),
            PixFmt::Rgb565 => (DrmFourcc::Rgb565, 16, 16),
        };
        for _ in 0..2 {
            let mut dumb = self.card.create_dumb_buffer((w, h), fourcc, bpp)?;
            let fb = match self.card.add_framebuffer(&dumb, depth, bpp) {
                Ok(fb) => fb,
                Err(e) => {
                    let _ = self.card.destroy_dumb_buffer(dumb);
                    return Err(e);
                }
            };
            let pitch = drm::buffer::Buffer::pitch(&dumb) as usize;
            let mut map = self.card.map_dumb_buffer(&mut dumb)?;
            let (ptr, len) = (map.as_mut_ptr(), map.len());
            // Kept mapped for the buffer's lifetime; unmapped in release.
            std::mem::forget(map);
            // Fault every page in now rather than during the first frames.
            unsafe { std::ptr::write_bytes(ptr, 0, len) };
            self.bufs.push(Buffer { dumb, fb, ptr, len, pitch });
        }
        self.buf_fmt = Some(fmt);
        Ok(())
    }

    /// Frees the framebuffers unless one is still being scanned out
    /// (removing it would switch the display off). Returns whether they're gone.
    pub fn release_buffers(&mut self) -> bool {
        if self.bufs.is_empty() {
            return true;
        }
        if self.is_showing_ours() {
            return false;
        }
        for b in self.bufs.drain(..) {
            unsafe { libc::munmap(b.ptr as *mut _, b.len) };
            let _ = self.card.destroy_framebuffer(b.fb);
            let _ = self.card.destroy_dumb_buffer(b.dumb);
        }
        self.buf_fmt = None;
        true
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
        let Some(out) = &self.output else { return false };
        match self.card.get_crtc(out.crtc).ok().and_then(|c| c.framebuffer()) {
            Some(fb) => self.bufs.iter().any(|b| b.fb == fb),
            None => false,
        }
    }

    /// Polls until someone else (fbcon, a GUI) has put their own framebuffer back.
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
        let out = self.output.as_ref().ok_or_else(|| other("no output"))?;
        let fb = self.bufs[self.back].fb;
        self.drain_events();
        let flipped = self
            .card
            .page_flip(out.crtc, fb, PageFlipFlags::EVENT, None)
            .and_then(|_| self.wait_flip());
        if flipped.is_err() {
            // Page flips can't change pixel format and aren't supported by
            // every driver; a legacy SETCRTC with the same mode still works.
            self.card.set_crtc(out.crtc, Some(fb), (0, 0), &out.conns, Some(out.mode))?;
        }
        self.back ^= 1;
        Ok(())
    }

    /// Discards flip events left over from an earlier timed-out wait, so
    /// they can't be mistaken for the next flip completing.
    fn drain_events(&self) {
        let mut pfd = libc::pollfd { fd: self.card.0.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        while unsafe { libc::poll(&mut pfd, 1, 0) } > 0 {
            if self.card.receive_events().is_err() {
                break;
            }
        }
    }

    fn wait_flip(&self) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_millis(200);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "page flip timed out"));
            }
            let mut pfd = libc::pollfd { fd: self.card.0.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            let n = unsafe { libc::poll(&mut pfd, 1, remaining.as_millis().max(1) as i32) };
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

    /// Reads whatever framebuffer is currently being scanned out (the
    /// foreground GUI session's frame), without involving the GUI itself.
    /// Works for any X or Wayland session as long as the buffer is linear;
    /// GPU-tiled/compressed buffers can't be read by the CPU.
    pub fn capture_scanout(&mut self, fmt: PixFmt) -> io::Result<Snapshot> {
        self.refresh_output()?;
        let crtc = self.output.as_ref().map(|o| o.crtc).ok_or_else(|| other("no output"))?;
        let fb = self.card.get_crtc(crtc)?.framebuffer().ok_or_else(|| other("nothing on screen"))?;
        let info = self.card.get_planar_framebuffer(fb).map_err(|e| other(format!("GETFB2: {e}")))?;

        let handles = info.buffers();
        let result = self.read_fb(&info, fmt);
        let mut closed = Vec::new();
        for h in handles.into_iter().flatten() {
            if !closed.contains(&h) {
                let _ = self.card.close_buffer(h);
                closed.push(h);
            }
        }
        result
    }

    fn read_fb(&self, info: &framebuffer::PlanarInfo, fmt: PixFmt) -> io::Result<Snapshot> {
        match info.modifier() {
            None | Some(DrmModifier::Linear) => {}
            Some(m) => return Err(other(format!("scanout buffer is GPU-tiled ({m:?})"))),
        }
        let (src_fmt, swap_rb) = match info.pixel_format() {
            DrmFourcc::Xrgb8888 | DrmFourcc::Argb8888 => (PixFmt::Xrgb8888, false),
            DrmFourcc::Xbgr8888 | DrmFourcc::Abgr8888 => (PixFmt::Xrgb8888, true),
            DrmFourcc::Rgb565 => (PixFmt::Rgb565, false),
            f => return Err(other(format!("unsupported scanout format {f:?}"))),
        };
        let handle = info.buffers()[0].ok_or_else(|| other("no permission to read the scanout buffer"))?;
        let (w, h) = info.size();
        let (w, h) = (w as usize, h as usize);
        let bpp = src_fmt.bytes_per_pixel();
        let pitch = info.pitches()[0] as usize;
        let offset = info.offsets()[0] as usize;
        let len = offset + pitch * (h - 1) + w * bpp;

        let dmabuf = self.card.buffer_to_prime_fd(handle, drm::CLOEXEC)?;
        let ptr = unsafe {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, dmabuf.as_raw_fd(), 0)
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        dma_buf_sync(dmabuf.as_raw_fd(), DMA_BUF_SYNC_START);
        let mem = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
        let row = w * bpp;
        let mut data = Vec::with_capacity(row * h);
        for y in 0..h {
            let start = offset + y * pitch;
            data.extend_from_slice(&mem[start..start + row]);
        }
        dma_buf_sync(dmabuf.as_raw_fd(), DMA_BUF_SYNC_END);
        unsafe { libc::munmap(ptr, len) };

        if swap_rb {
            for px in data.chunks_exact_mut(4) {
                px.swap(0, 2);
            }
        }
        Ok(Snapshot { width: w as u32, height: h as u32, fmt: src_fmt, data }.to_fmt(fmt))
    }
}

const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x4008_6200;
const DMA_BUF_SYNC_READ: u64 = 1;
const DMA_BUF_SYNC_START: u64 = 0;
const DMA_BUF_SYNC_END: u64 = 4;

/// Brackets CPU reads so non-coherent caches see the GPU's writes.
fn dma_buf_sync(fd: i32, phase: u64) {
    let flags: u64 = DMA_BUF_SYNC_READ | phase;
    unsafe { libc::ioctl(fd, DMA_BUF_IOCTL_SYNC, &flags) };
}

impl Drop for Kms {
    fn drop(&mut self) {
        self.release_buffers();
    }
}
