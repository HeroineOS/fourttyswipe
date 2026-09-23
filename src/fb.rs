//! Read-only `/dev/fb0` capture of the text console. Nothing here ever
//! writes to the framebuffer: all drawing goes through our own KMS
//! buffers (`kms.rs`), so fbcon's content can't be corrupted.

use std::fs::OpenOptions;
use std::io;
use std::os::unix::io::AsRawFd;

use crate::snapshot::{PixFmt, Snapshot};

const FBIOGET_VSCREENINFO: libc::c_ulong = 0x4600;
const FBIOGET_FSCREENINFO: libc::c_ulong = 0x4602;

// Matches the stable UAPI in <linux/fb.h>; `unsigned long` is 8 bytes on
// the LP64 targets we ship (aarch64, x86_64).
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

struct Info {
    var: FbVarScreeninfo,
    fix: FbFixScreeninfo,
}

fn query(fd: i32) -> io::Result<Info> {
    let mut var = FbVarScreeninfo::default();
    if unsafe { libc::ioctl(fd, FBIOGET_VSCREENINFO, &mut var) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: plain-old-data C struct, all-zero is a valid value.
    let mut fix: FbFixScreeninfo = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, FBIOGET_FSCREENINFO, &mut fix) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Info { var, fix })
}

fn pixfmt(var: &FbVarScreeninfo) -> Option<PixFmt> {
    match var.bits_per_pixel {
        32 if var.red.offset == 16 && var.green.offset == 8 && var.blue.offset == 0 => {
            Some(PixFmt::Xrgb8888)
        }
        16 if var.red.offset == 11 && var.green.offset == 5 && var.blue.offset == 0 => {
            Some(PixFmt::Rgb565)
        }
        _ => None,
    }
}

fn unsupported() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, "unsupported fbdev pixel format")
}

pub fn format() -> io::Result<PixFmt> {
    let file = OpenOptions::new().read(true).open("/dev/fb0")?;
    let info = query(file.as_raw_fd())?;
    pixfmt(&info.var).ok_or_else(unsupported)
}

/// Snapshots the visible area of the text console.
pub fn capture() -> io::Result<Snapshot> {
    let file = OpenOptions::new().read(true).open("/dev/fb0")?;
    let fd = file.as_raw_fd();
    let Info { var, fix } = query(fd)?;
    let fmt = pixfmt(&var).ok_or_else(unsupported)?;

    let bpp = fmt.bytes_per_pixel();
    let (w, h) = (var.xres as usize, var.yres as usize);
    let pitch = fix.line_length as usize;
    let origin = var.yoffset as usize * pitch + var.xoffset as usize * bpp;
    let len = fix.smem_len as usize;
    if w == 0 || h == 0 || origin + (h - 1) * pitch + w * bpp > len {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad fbdev geometry"));
    }

    let ptr = unsafe {
        libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, fd, 0)
    };
    if ptr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    let mem = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
    let row = w * bpp;
    let mut data = Vec::with_capacity(row * h);
    for y in 0..h {
        let start = origin + y * pitch;
        data.extend_from_slice(&mem[start..start + row]);
    }
    unsafe { libc::munmap(ptr, len) };

    Ok(Snapshot { width: w as u32, height: h as u32, fmt, data })
}
