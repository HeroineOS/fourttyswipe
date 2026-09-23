//! Detects whether a VT is in text (fbcon) or graphics mode, using the same
//! kernel mechanism Xorg/Wayland use to mark a VT as taken over
//! (`KDSETMODE`/`KDGETMODE`, `KD_GRAPHICS` vs `KD_TEXT`). This is the
//! correct, generic way to know "is this a plain console I can safely
//! animate on" vs "a GUI session where a raw fbdev read/write is
//! meaningless or actively harmful (stale/garbage buffer content)".

use std::fs::OpenOptions;
use std::io;
use std::os::unix::io::AsRawFd;

const KDGETMODE: libc::c_ulong = 0x4B3B;
const KD_TEXT: libc::c_int = 0;

/// True if VT `vt` is currently in graphics mode (a compositor/X server has
/// taken it over), false if it's a plain text console.
pub fn is_graphics_mode(vt: u16) -> io::Result<bool> {
    let path = format!("/dev/tty{vt}");
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    let mut mode: libc::c_int = 0;
    let ret = unsafe { libc::ioctl(file.as_raw_fd(), KDGETMODE, &mut mode) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(mode != KD_TEXT)
}
