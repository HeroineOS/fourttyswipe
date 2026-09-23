//! Captures an X server's screen over the X11 protocol, for VTs where a
//! GUI session owns the display and fbdev can't see its content.

use std::fs;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::shm::ConnectionExt as ShmExt;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, ImageOrder};
use x11rb::rust_connection::{DefaultStream, RustConnection};

use crate::snapshot::{PixFmt, Snapshot};

const TIMEOUT: Duration = Duration::from_millis(1500);

struct XServer {
    display: u32,
    auth_file: Option<PathBuf>,
}

fn find_server(vt: u16) -> Option<XServer> {
    let tty = format!("/dev/tty{vt}");
    for entry in fs::read_dir("/proc").ok()?.flatten() {
        let pid = entry.file_name();
        let Some(pid) = pid.to_str().filter(|p| p.bytes().all(|b| b.is_ascii_digit())) else { continue };
        let comm = fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        if !matches!(comm.trim(), "Xorg" | "X") {
            continue;
        }
        let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) else { continue };
        let args: Vec<String> = cmdline
            .split(|&b| b == 0)
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();

        let mut display = None;
        let mut auth_file = None;
        let mut on_vt = None;
        for (i, a) in args.iter().enumerate().skip(1) {
            if let Some(n) = a.strip_prefix(':').and_then(|n| n.parse().ok()) {
                display = Some(n);
            } else if a == "-auth" {
                auth_file = args.get(i + 1).map(PathBuf::from);
            } else if let Some(n) = a.strip_prefix("vt").and_then(|n| n.parse::<u16>().ok()) {
                on_vt = Some(n);
            }
        }
        let on_vt = on_vt.map(|n| n == vt).unwrap_or_else(|| holds_tty(pid, &tty));
        if on_vt {
            return Some(XServer { display: display.unwrap_or(0), auth_file });
        }
    }
    None
}

fn holds_tty(pid: &str, tty: &str) -> bool {
    let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else { return false };
    fds.flatten()
        .any(|fd| fs::read_link(fd.path()).map(|p| p.as_os_str() == tty).unwrap_or(false))
}

/// Parses an Xauthority file for a MIT-MAGIC-COOKIE-1 matching `display`.
fn read_cookie(path: &PathBuf, display: u32) -> Option<(Vec<u8>, Vec<u8>)> {
    let buf = fs::read(path).ok()?;
    let mut pos = 0;
    let field = |pos: &mut usize| -> Option<Vec<u8>> {
        let len = u16::from_be_bytes([*buf.get(*pos)?, *buf.get(*pos + 1)?]) as usize;
        let v = buf.get(*pos + 2..*pos + 2 + len)?.to_vec();
        *pos += 2 + len;
        Some(v)
    };
    let want = display.to_string().into_bytes();
    let mut fallback = None;
    while pos + 2 <= buf.len() {
        pos += 2; // family
        let _address = field(&mut pos)?;
        let number = field(&mut pos)?;
        let name = field(&mut pos)?;
        let data = field(&mut pos)?;
        if name != b"MIT-MAGIC-COOKIE-1" {
            continue;
        }
        if number == want {
            return Some((name, data));
        }
        fallback.get_or_insert((name, data));
    }
    fallback
}

fn connect(server: &XServer) -> Option<RustConnection> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::SocketAddr;

    let path = format!("/tmp/.X11-unix/X{}", server.display);
    let stream = UnixStream::connect(&path).ok().or_else(|| {
        let addr = SocketAddr::from_abstract_name(path.as_bytes()).ok()?;
        UnixStream::connect_addr(&addr).ok()
    })?;
    let (stream, _) = DefaultStream::from_unix_stream(stream).ok()?;
    let (name, data) = server
        .auth_file
        .as_ref()
        .and_then(|p| read_cookie(p, server.display))
        .unwrap_or_default();
    RustConnection::connect_to_stream_with_auth_info(stream, 0, name, data).ok()
}

fn grab(conn: &RustConnection) -> Option<Snapshot> {
    let setup = conn.setup();
    let screen = setup.roots.first()?;
    let (w, h) = (screen.width_in_pixels, screen.height_in_pixels);
    let depth = screen.root_depth;
    let format = setup.pixmap_formats.iter().find(|f| f.depth == depth)?;
    let fmt = match (depth, format.bits_per_pixel) {
        (24 | 32, 32) => PixFmt::Xrgb8888,
        (16, 16) => PixFmt::Rgb565,
        _ => return None,
    };
    if setup.image_byte_order != ImageOrder::LSB_FIRST {
        return None;
    }
    let pad = format.scanline_pad as usize;
    let stride = ((w as usize * format.bits_per_pixel as usize + pad - 1) / pad) * pad / 8;
    let size = stride * h as usize;

    let raw = grab_shm(conn, screen.root, w, h, size).or_else(|| {
        let reply = conn
            .get_image(ImageFormat::Z_PIXMAP, screen.root, 0, 0, w, h, !0)
            .ok()?
            .reply()
            .ok()?;
        Some(reply.data)
    })?;
    if raw.len() < size {
        return None;
    }

    let row = w as usize * fmt.bytes_per_pixel();
    let mut data = Vec::with_capacity(row * h as usize);
    for y in 0..h as usize {
        data.extend_from_slice(&raw[y * stride..y * stride + row]);
    }
    Some(Snapshot { width: w as u32, height: h as u32, fmt, data })
}

/// MIT-SHM via a memfd: avoids pushing the whole frame through the socket,
/// and the fd-passing variant needs no world-accessible SysV segment.
fn grab_shm(conn: &RustConnection, root: u32, w: u16, h: u16, size: usize) -> Option<Vec<u8>> {
    conn.shm_query_version().ok()?.reply().ok()?;
    let fd = unsafe { libc::memfd_create(c"fourttyswipe-shm".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let memfd = unsafe { OwnedFd::from_raw_fd(fd) };
    if unsafe { libc::ftruncate(memfd.as_raw_fd(), size as libc::off_t) } != 0 {
        return None;
    }
    let seg = conn.generate_id().ok()?;
    conn.shm_attach_fd(seg, memfd.try_clone().ok()?, false).ok()?;
    let result = conn
        .shm_get_image(root, 0, 0, w, h, !0, ImageFormat::Z_PIXMAP.into(), seg, 0)
        .ok()
        .and_then(|c| c.reply().ok())
        .and_then(|_| {
            let ptr = unsafe {
                libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ, libc::MAP_SHARED, memfd.as_raw_fd(), 0)
            };
            if ptr == libc::MAP_FAILED {
                return None;
            }
            let out = unsafe { std::slice::from_raw_parts(ptr as *const u8, size) }.to_vec();
            unsafe { libc::munmap(ptr, size) };
            Some(out)
        });
    let _ = conn.shm_detach(seg);
    let _ = conn.flush();
    result
}

/// Screenshot of the X server running on `vt`, or `None` if there isn't
/// one we can reach. Runs on a helper thread so a wedged X server can't
/// stall the transition worker.
pub fn capture(vt: u16) -> Option<Snapshot> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let snap = find_server(vt).and_then(|s| connect(&s)).and_then(|c| grab(&c));
        let _ = tx.send(snap);
    });
    rx.recv_timeout(TIMEOUT).ok().flatten().filter(|s| !s.is_blank())
}
