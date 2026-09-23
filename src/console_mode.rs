//! Text vs graphics mode of a VT (`KDGETMODE`/`KDSETMODE`) — the same flag
//! Xorg/Wayland set when they take a VT over.

use std::fs::OpenOptions;
use std::io;
use std::os::unix::io::AsRawFd;

const KDSETMODE: libc::c_ulong = 0x4B3A;
const KDGETMODE: libc::c_ulong = 0x4B3B;
const KD_TEXT: libc::c_int = 0;
const KD_GRAPHICS: libc::c_int = 1;

fn open(vt: u16) -> io::Result<std::fs::File> {
    OpenOptions::new().read(true).write(true).open(format!("/dev/tty{vt}"))
}

pub fn is_graphics_mode(vt: u16) -> io::Result<bool> {
    let file = open(vt)?;
    let mut mode: libc::c_int = 0;
    if unsafe { libc::ioctl(file.as_raw_fd(), KDGETMODE, &mut mode) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(mode != KD_TEXT)
}

/// Records a text VT we've temporarily put into graphics mode, so a crash
/// mid-transition can't leave it stuck there (see [`restore_borrowed`]).
const BORROWED_MARKER: &str = "/run/fourttyswipe-borrowed-vt";

/// On the foreground VT, going graphics->text makes the kernel force fbcon
/// to reclaim the display and fully redraw — that's how we hand the screen
/// back after an animation. On a background VT it just records the mode.
///
/// Only ever called on text consoles we're borrowing, never on a GUI VT.
pub fn set_graphics_mode(vt: u16, graphics: bool) -> io::Result<()> {
    if graphics {
        let _ = std::fs::write(BORROWED_MARKER, vt.to_string());
    }
    let file = open(vt)?;
    let mode = if graphics { KD_GRAPHICS } else { KD_TEXT };
    if unsafe { libc::ioctl(file.as_raw_fd(), KDSETMODE, mode as libc::c_ulong) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if !graphics {
        let _ = std::fs::remove_file(BORROWED_MARKER);
    }
    Ok(())
}

/// Puts back a text VT a previous run left in graphics mode (crash or kill
/// mid-transition).
pub fn restore_borrowed() {
    let Ok(s) = std::fs::read_to_string(BORROWED_MARKER) else { return };
    if let Ok(vt) = s.trim().parse::<u16>() {
        eprintln!("fourttyswipe: restoring VT{vt} to text mode after an interrupted transition");
        let _ = set_graphics_mode(vt, false);
    }
    let _ = std::fs::remove_file(BORROWED_MARKER);
}
