//! Direct `/dev/fb0` access for the transition animation.
//!
//! Scope/limitation, read before touching this: this only works for a VT
//! that's a plain text console (fbcon) with no GPU compositor holding DRM
//! master over it. An active Xorg or Wayland (HeroiWM) session normally
//! takes DRM master and the fbdev-emulation layer goes inactive while it
//! does, so writes here are a no-op (harmless, just doesn't animate) on
//! those VTs. Capturing a *GUI* session's real content needs a
//! session-specific mechanism instead — X's composite extension, or a
//! Wayland screencopy protocol implemented in HeroiWM — and is future work,
//! not something a generic fbdev capture can portably do.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;

const FBIOGET_VSCREENINFO: libc::c_ulong = 0x4600;
const FBIOGET_FSCREENINFO: libc::c_ulong = 0x4602;

// Layout matches the stable Linux UAPI in <linux/fb.h>. `unsigned long`
// fields are 8 bytes on both our LP64 targets (aarch64, x86_64).
#[repr(C)]
#[derive(Default)]
struct FbBitfield {
    offset: u32,
    length: u32,
    msb_right: u32,
}

#[repr(C)]
#[derive(Default)]
struct FbVarScreeninfo {
    xres: u32,
    yres: u32,
    xres_virtual: u32,
    yres_virtual: u32,
    xoffset: u32,
    yoffset: u32,
    bits_per_pixel: u32,
    grayscale: u32,
    red: FbBitfield,
    green: FbBitfield,
    blue: FbBitfield,
    transp: FbBitfield,
    nonstd: u32,
    activate: u32,
    height: u32,
    width: u32,
    accel_flags: u32,
    pixclock: u32,
    left_margin: u32,
    right_margin: u32,
    upper_margin: u32,
    lower_margin: u32,
    hsync_len: u32,
    vsync_len: u32,
    sync: u32,
    vmode: u32,
    rotate: u32,
    colorspace: u32,
    reserved: [u32; 4],
}

#[repr(C)]
struct FbFixScreeninfo {
    id: [u8; 16],
    smem_start: u64,
    smem_len: u32,
    fb_type: u32,
    type_aux: u32,
    visual: u32,
    xpanstep: u16,
    ypanstep: u16,
    ywrapstep: u16,
    line_length: u32,
    mmio_start: u64,
    mmio_len: u32,
    accel: u32,
    capabilities: u16,
    reserved: [u16; 2],
}

impl Default for FbFixScreeninfo {
    fn default() -> Self {
        // SAFETY: an all-zero fb_fix_screeninfo is a valid bit pattern
        // (plain-old-data C struct, no padding/alignment requirements
        // beyond u64/u32/u16/u8 fields).
        unsafe { std::mem::zeroed() }
    }
}

pub struct Geometry {
    pub width: u32,
    pub height: u32,
    pub bits_per_pixel: u32,
    /// Bytes per scanline row (may exceed `width * bytes_per_pixel` due to
    /// hardware padding — always index rows by this, not a computed stride).
    pub line_length: u32,
    pub smem_len: u32,
}

pub struct Framebuffer {
    file: File,
    pub geometry: Geometry,
}

impl Framebuffer {
    pub fn open() -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open("/dev/fb0")?;

        let mut var = FbVarScreeninfo::default();
        let ret = unsafe { libc::ioctl(file.as_raw_fd(), FBIOGET_VSCREENINFO, &mut var) };
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }

        let mut fix = FbFixScreeninfo::default();
        let ret = unsafe { libc::ioctl(file.as_raw_fd(), FBIOGET_FSCREENINFO, &mut fix) };
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            file,
            geometry: Geometry {
                width: var.xres,
                height: var.yres,
                bits_per_pixel: var.bits_per_pixel,
                line_length: fix.line_length,
                smem_len: fix.smem_len,
            },
        })
    }

    fn mmap(&self) -> io::Result<*mut u8> {
        let len = self.geometry.smem_len as usize;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                self.file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(ptr as *mut u8)
    }

    /// Snapshots the current screen contents. Returns raw pixel bytes in
    /// whatever native format the console is using (typically RGB565 or
    /// XRGB8888 — we don't reinterpret it, just capture/replay verbatim).
    pub fn capture(&self) -> io::Result<Vec<u8>> {
        let len = self.geometry.smem_len as usize;
        let ptr = self.mmap()?;
        let snapshot = unsafe { std::slice::from_raw_parts(ptr, len).to_vec() };
        unsafe {
            libc::munmap(ptr as *mut _, len);
        }
        Ok(snapshot)
    }

    /// Writes a full frame buffer back to the screen. `frame` must be
    /// exactly `geometry.smem_len` bytes, laid out the same as `capture()`
    /// returned it.
    pub fn blit(&self, frame: &[u8]) -> io::Result<()> {
        let len = self.geometry.smem_len as usize;
        if frame.len() != len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "frame size does not match framebuffer size",
            ));
        }
        let ptr = self.mmap()?;
        unsafe {
            std::ptr::copy_nonoverlapping(frame.as_ptr(), ptr, len);
            libc::munmap(ptr as *mut _, len);
        }
        Ok(())
    }
}
