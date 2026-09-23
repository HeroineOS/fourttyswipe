//! Slide transition between VTs.
//!
//! The animation is shown from our own KMS framebuffers (`kms.rs`) while
//! the real VT switch happens underneath, then the display is handed back
//! to whoever owns the target VT on a frame identical to what they'll show:
//!
//! - text target: the kernel forces fbcon to reclaim + redraw the display
//!   when the foreground VT goes graphics -> text (`KDSETMODE`).
//! - GUI target: we drop DRM master and the X server takes the display
//!   back itself when its VT becomes active.
//!
//! Text consoles are captured from fbdev, X sessions over the X11 protocol.
//! Every failure path falls back to a plain instant switch.

use std::collections::HashMap;
use std::thread::sleep;
use std::time::{Duration, Instant};

use fourswipe_core::SwipeDirection;

use crate::console_mode::{is_graphics_mode, restore_borrowed, set_graphics_mode};
use crate::kms::Kms;
use crate::snapshot::Snapshot;
use crate::vt::VtSwitcher;
use crate::{fb, xcapture};

const DURATION: Duration = Duration::from_millis(240);
const MASTER_TIMEOUT: Duration = Duration::from_millis(500);
const RECLAIM_TIMEOUT: Duration = Duration::from_millis(300);
const GUI_RECLAIM_TIMEOUT: Duration = Duration::from_secs(3);

type Result<T> = std::result::Result<T, String>;

pub struct Transitioner {
    vt: VtSwitcher,
    /// Last known image of each text console, used when we need its
    /// content before fbcon has drawn it (GUI -> text).
    tty_cache: HashMap<u16, Snapshot>,
    /// Last known image of each GUI session, used when a backgrounded X
    /// server won't hand back its screen (text -> GUI).
    x_cache: HashMap<u16, Snapshot>,
}

impl Transitioner {
    pub fn new(vt: VtSwitcher) -> Self {
        restore_borrowed();
        let mut t = Self { vt, tty_cache: HashMap::new(), x_cache: HashMap::new() };
        if let Ok(active) = t.vt.active_vt() {
            match is_graphics_mode(active) {
                Ok(false) => {
                    if let Ok(s) = fb::capture() {
                        t.tty_cache.insert(active, s);
                    }
                }
                Ok(true) => {
                    if let Some(s) = xcapture::capture(active) {
                        t.x_cache.insert(active, s);
                    }
                }
                Err(_) => {}
            }
        }
        t
    }

    pub fn swipe(&mut self, direction: SwipeDirection) {
        let forward = direction == SwipeDirection::Left;
        let (from, to) = match self.vt.targets(forward) {
            Ok(t) => t,
            Err(e) => return eprintln!("fourttyswipe: can't read VT state: {e}"),
        };
        if from == to {
            return;
        }
        let from_gui = is_graphics_mode(from).unwrap_or(true);
        let to_gui = is_graphics_mode(to).unwrap_or(true);

        let result = match (from_gui, to_gui) {
            (false, false) => self.text_to_text(from, to, direction),
            (false, true) => self.text_to_gui(from, to, direction),
            (true, false) => self.gui_to_text(from, to, direction),
            (true, true) => Err("GUI to GUI switches aren't animated".into()),
        };

        if let Err(why) = result {
            eprintln!("fourttyswipe: VT{from} -> VT{to} not animated: {why}");
        }
        if self.vt.active_vt().ok() != Some(to) {
            if let Err(e) = self.vt.activate(to) {
                return eprintln!("fourttyswipe: VT switch failed: {e}");
            }
        }
        println!("fourttyswipe: switched to VT{to}");
    }

    fn text_to_text(&mut self, from: u16, to: u16, dir: SwipeDirection) -> Result<()> {
        let from_img = fb::capture().map_err(|e| format!("capture VT{from}: {e}"))?;
        self.tty_cache.insert(from, from_img.clone());
        let mut kms = take_display(&from_img)?;

        // The screen is frozen on our copy of VT `from`; fbcon draws `to`
        // into its own (now hidden) buffer during the switch.
        let result = (|| -> Result<()> {
            self.vt.activate(to).map_err(|e| format!("switch: {e}"))?;
            let to_img = fb::capture().map_err(|e| format!("capture VT{to}: {e}"))?;
            check_size(&kms, &to_img)?;
            self.tty_cache.insert(to, to_img.clone());
            animate(&mut kms, &from_img, &to_img, dir);
            Ok(())
        })();

        let fg = self.vt.active_vt().unwrap_or(from);
        handoff_to_text(kms, fg);
        result
    }

    fn text_to_gui(&mut self, from: u16, to: u16, dir: SwipeDirection) -> Result<()> {
        let to_img = match xcapture::capture(to) {
            Some(img) => {
                self.x_cache.insert(to, img.clone());
                img
            }
            None => self
                .x_cache
                .get(&to)
                .cloned()
                .ok_or_else(|| format!("no screenshot of the GUI session on VT{to} yet"))?,
        };
        let from_img = fb::capture().map_err(|e| format!("capture VT{from}: {e}"))?;
        self.tty_cache.insert(from, from_img.clone());
        let to_img = to_img.to_fmt(from_img.fmt);
        if (to_img.width, to_img.height) != (from_img.width, from_img.height) {
            return Err("GUI and console resolutions differ".into());
        }

        let mut kms = take_display(&from_img)?;
        animate(&mut kms, &from_img, &to_img, dir);

        kms.drop_master();
        if let Err(e) = self.vt.activate(to) {
            handoff_to_text(kms, from);
            return Err(format!("switch: {e}"));
        }
        if !kms.wait_released(GUI_RECLAIM_TIMEOUT) {
            eprintln!("fourttyswipe: GUI on VT{to} didn't reclaim the display in time");
        }
        Ok(())
    }

    fn gui_to_text(&mut self, from: u16, to: u16, dir: SwipeDirection) -> Result<()> {
        let fmt = fb::format().map_err(|e| format!("fbdev: {e}"))?;
        let from_img = xcapture::capture(from)
            .ok_or_else(|| format!("couldn't screenshot the GUI session on VT{from}"))?
            .to_fmt(fmt);
        self.x_cache.insert(from, from_img.clone());

        let mut kms = Kms::open(fmt).map_err(|e| format!("DRM: {e}"))?;
        check_size(&kms, &from_img)?;
        kms.draw_snapshot(&from_img);

        let cached = self.tty_cache.get(&to).cloned().filter(|c| {
            (c.width, c.height, c.fmt) == (from_img.width, from_img.height, fmt)
        });

        match cached {
            Some(to_img) => {
                // Keeping the target in graphics mode through the switch
                // stops fbcon from reclaiming (and flashing) the display,
                // and grabbing master the moment X releases it keeps
                // anything else from drawing. X's last frame stays up
                // until our identical copy replaces it.
                set_graphics_mode(to, true).map_err(|e| format!("KDSETMODE: {e}"))?;
                if let Err(e) = self.vt.request(to) {
                    let _ = set_graphics_mode(to, false);
                    return Err(format!("switch: {e}"));
                }
                let got_master = kms.acquire_master(MASTER_TIMEOUT);
                let presented = got_master && kms.present().is_ok();
                let _ = self.vt.wait_active(to);
                if !presented {
                    handoff_to_text(kms, to);
                    return Err("couldn't take over the display".into());
                }
                animate(&mut kms, &from_img, &to_img, dir);
                handoff_to_text(kms, to);
                if let Ok(s) = fb::capture() {
                    self.tty_cache.insert(to, s);
                }
                Ok(())
            }
            None => {
                // Without a prior image of this console, fbcon has to draw it
                // once before it can be captured, so it may flash for a frame.
                self.vt.activate(to).map_err(|e| format!("switch: {e}"))?;
                if !kms.acquire_master(MASTER_TIMEOUT) {
                    return Err("couldn't become DRM master".into());
                }
                if let Err(e) = kms.present() {
                    kms.drop_master();
                    return Err(format!("present: {e}"));
                }
                let to_img = match fb::capture() {
                    Ok(img) => img,
                    Err(e) => {
                        handoff_to_text(kms, to);
                        return Err(format!("capture VT{to}: {e}"));
                    }
                };
                self.tty_cache.insert(to, to_img.clone());
                animate(&mut kms, &from_img, &to_img, dir);
                handoff_to_text(kms, to);
                Ok(())
            }
        }
    }
}

fn check_size(kms: &Kms, img: &Snapshot) -> Result<()> {
    if (kms.width, kms.height) != (img.width, img.height) {
        return Err(format!(
            "display is {}x{} but capture is {}x{}",
            kms.width, kms.height, img.width, img.height
        ));
    }
    Ok(())
}

/// From a text VT (no DRM master held): show an identical copy of the
/// current screen from our own buffer.
fn take_display(img: &Snapshot) -> Result<Kms> {
    let mut kms = Kms::open(img.fmt).map_err(|e| format!("DRM: {e}"))?;
    check_size(&kms, img)?;
    if !kms.acquire_master(MASTER_TIMEOUT) {
        return Err("couldn't become DRM master".into());
    }
    kms.draw_snapshot(img);
    if let Err(e) = kms.present() {
        kms.drop_master();
        return Err(format!("present: {e}"));
    }
    Ok(kms)
}

/// Gives the display back to fbcon on foreground text VT `vt`. Flipping the
/// VT graphics -> text makes the kernel force fbcon's framebuffer back onto
/// the CRTC and redraw, regardless of DRM master.
fn handoff_to_text(kms: Kms, vt: u16) {
    for _ in 0..2 {
        let _ = set_graphics_mode(vt, true);
        kms.drop_master();
        let _ = set_graphics_mode(vt, false);
        if kms.wait_released(RECLAIM_TIMEOUT) {
            return;
        }
    }
    eprintln!("fourttyswipe: fbcon didn't reclaim the display on VT{vt}");
}

fn animate(kms: &mut Kms, from: &Snapshot, to: &Snapshot, dir: SwipeDirection) {
    let start = Instant::now();
    loop {
        let t = (start.elapsed().as_secs_f64() / DURATION.as_secs_f64()).min(1.0);
        let eased = 1.0 - (1.0 - t).powi(3);
        compose(kms, from, to, dir, eased);
        let frame = Instant::now();
        if kms.present().is_err() {
            break;
        }
        // Page flips pace us at vblank; if the driver fell back to
        // unsynchronized SETCRTC, pace manually instead of spinning.
        if let Some(rest) = Duration::from_millis(16).checked_sub(frame.elapsed()) {
            if rest > Duration::from_millis(12) {
                sleep(rest);
            }
        }
        if t >= 1.0 {
            break;
        }
    }
}

/// Draws one frame: `from` sliding out in `dir`, `to` sliding in behind it.
fn compose(kms: &mut Kms, from: &Snapshot, to: &Snapshot, dir: SwipeDirection, progress: f64) {
    let (w, h) = (from.width as usize, from.height as usize);
    let bpp = from.fmt.bytes_per_pixel();
    let rb = w * bpp;
    let (mem, pitch) = kms.back_buffer();

    match dir {
        SwipeDirection::Left | SwipeDirection::Right => {
            let ob = ((progress * w as f64).round() as usize).min(w) * bpp;
            for y in 0..h {
                let dst = &mut mem[y * pitch..y * pitch + rb];
                let (f, t) = (from.row(y), to.row(y));
                if dir == SwipeDirection::Left {
                    dst[..rb - ob].copy_from_slice(&f[ob..]);
                    dst[rb - ob..].copy_from_slice(&t[..ob]);
                } else {
                    dst[..ob].copy_from_slice(&t[rb - ob..]);
                    dst[ob..].copy_from_slice(&f[..rb - ob]);
                }
            }
        }
        SwipeDirection::Up | SwipeDirection::Down => {
            let off = ((progress * h as f64).round() as usize).min(h);
            for y in 0..h {
                let src = if dir == SwipeDirection::Up {
                    if y < h - off { from.row(y + off) } else { to.row(y - (h - off)) }
                } else if y < off {
                    to.row(y + (h - off))
                } else {
                    from.row(y - off)
                };
                mem[y * pitch..y * pitch + rb].copy_from_slice(src);
            }
        }
    }
}
