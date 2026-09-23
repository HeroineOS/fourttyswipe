//! Kernel VT (virtual terminal) switching via `/dev/tty0` ioctls — the same
//! mechanism Ctrl+Alt+F<N> uses. This is intentionally independent of
//! anything running *on* a VT: a getty shell, Xorg, a Wayland compositor
//! (HeroiWM or otherwise), all just happen to own whichever VT is currently
//! active. Switching VTs at this level works the same regardless of what's
//! running on either end, and regardless of whether that session's
//! compositor has any concept of touch input at all — tty-swipe reads the
//! touchscreen itself via fourswipe-core's evdev backend and never
//! goes through a compositor's input pipeline.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;

const VT_GETSTATE: libc::c_ulong = 0x5603;
const VT_ACTIVATE: libc::c_ulong = 0x5606;
const VT_WAITACTIVE: libc::c_ulong = 0x5607;

#[repr(C)]
#[derive(Default)]
struct VtStat {
    v_active: u16,
    v_signal: u16,
    v_state: u16,
}

pub struct VtSwitcher {
    fd: File,
}

impl VtSwitcher {
    /// Opens `/dev/tty0`, the kernel's "current VT" control device.
    /// Requires root (or `CAP_SYS_TTY_CONFIG` + appropriate device perms).
    pub fn open() -> io::Result<Self> {
        let fd = OpenOptions::new().read(true).write(true).open("/dev/tty0")?;
        Ok(Self { fd })
    }

    /// Returns (currently active VT, all allocated/in-use VTs).
    ///
    /// Note: `VT_GETSTATE`'s state bitmask is 16 bits (kernel limitation),
    /// so this only sees VTs 1-15. Fine for the default 6-getty setup most
    /// distros ship; a wider scheme would need per-VT `/dev/ttyN` probing.
    fn state(&self) -> io::Result<(u16, Vec<u16>)> {
        let mut stat = VtStat::default();
        let ret = unsafe { libc::ioctl(self.fd.as_raw_fd(), VT_GETSTATE, &mut stat as *mut VtStat) };
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }
        let allocated = (1u16..16).filter(|vt| stat.v_state & (1 << vt) != 0).collect();
        Ok((stat.v_active, allocated))
    }

    pub fn active_vt(&self) -> io::Result<u16> {
        Ok(self.state()?.0)
    }

    /// Asks the kernel to switch to `vt` without waiting for it to finish
    /// (a GUI session on the current VT has to release it first).
    pub fn request(&self, vt: u16) -> io::Result<()> {
        if unsafe { libc::ioctl(self.fd.as_raw_fd(), VT_ACTIVATE, vt as libc::c_ulong) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn wait_active(&self, vt: u16) -> io::Result<()> {
        if unsafe { libc::ioctl(self.fd.as_raw_fd(), VT_WAITACTIVE, vt as libc::c_ulong) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Switches to `vt` and blocks until the kernel reports it active.
    pub fn activate(&self, vt: u16) -> io::Result<()> {
        self.request(vt)?;
        self.wait_active(vt)
    }

    /// Returns (current VT, next or previous allocated VT, wrapping around).
    pub fn targets(&self, forward: bool) -> io::Result<(u16, u16)> {
        let (active, allocated) = self.state()?;
        if allocated.is_empty() {
            return Ok((active, active));
        }
        let pos = allocated.iter().position(|&v| v == active).unwrap_or(0);
        let len = allocated.len();
        let next_pos = if forward { (pos + 1) % len } else { (pos + len - 1) % len };
        Ok((active, allocated[next_pos]))
    }
}
