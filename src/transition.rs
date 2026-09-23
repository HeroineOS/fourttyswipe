//! Slide transition between VTs.
//!
//! The animation is shown from our own KMS framebuffers (`kms.rs`) while
//! the real VT switch happens underneath, then the display is handed back
//! to whoever owns the target VT on a frame identical to what they'll show:
//!
//! - text target: the kernel forces fbcon to reclaim + redraw the display
//!   when the foreground VT goes graphics -> text (`KDSETMODE`).
//! - GUI target: we drop DRM master and the session takes the display back
//!   itself when its VT becomes active.
//!
//! Latency: everything slow (allocating buffers, reading the current screen)
//! happens in [`Transitioner::prepare`], triggered when four fingers touch
//! down — i.e. while the user is still swiping. Incoming screens come from
//! caches refreshed every time a VT is left, so the animation can start the
//! moment the swipe is recognized.

use std::collections::HashMap;
use std::thread::sleep;
use std::time::{Duration, Instant};

use fourswipe_core::SwipeDirection;

use crate::console_mode::{is_graphics_mode, restore_borrowed, set_graphics_mode};
use crate::kms::Kms;
use crate::snapshot::{PixFmt, Snapshot};
use crate::vt::VtSwitcher;
use crate::{fb, xcapture};

const DURATION: Duration = Duration::from_millis(240);
const MASTER_TIMEOUT: Duration = Duration::from_millis(500);
const RECLAIM_TIMEOUT: Duration = Duration::from_millis(300);
const GUI_RECLAIM_TIMEOUT: Duration = Duration::from_secs(3);
/// A screenshot taken at touch-down is still used if the swipe completes
/// within this long.
const PREPARED_MAX_AGE: Duration = Duration::from_secs(4);
/// Buffers (2 screens' worth of memory) are freed after this much idle time.
const IDLE_RELEASE: Duration = Duration::from_secs(30);
/// A GUI that was just handed the display may not have drawn yet; don't
/// cache its screen this soon after.
const GUI_SETTLE: Duration = Duration::from_millis(800);

type Result<T> = std::result::Result<T, String>;

struct Prepared {
    vt: u16,
    img: Snapshot,
    at: Instant,
}

#[derive(Default)]
struct Timing {
    frames: u32,
    late: u32,
    worst: Duration,
}

pub struct Transitioner {
    vt: VtSwitcher,
    kms: Option<Kms>,
    fmt: PixFmt,
    /// Last known image of each text console.
    tty_cache: HashMap<u16, Snapshot>,
    /// Last known image of each GUI session (X or Wayland).
    gui_cache: HashMap<u16, Snapshot>,
    prepared: Option<Prepared>,
    last_used: Instant,
    last_switch: Instant,
}

impl Transitioner {
    pub fn new(vt: VtSwitcher) -> Self {
        restore_borrowed();
        let fmt = fb::format().unwrap_or(PixFmt::Xrgb8888);
        let mut t = Self {
            vt,
            kms: None,
            fmt,
            tty_cache: HashMap::new(),
            gui_cache: HashMap::new(),
            prepared: None,
            last_used: Instant::now(),
            last_switch: Instant::now() - GUI_SETTLE,
        };
        if let Ok(active) = t.vt.active_vt() {
            // At boot a GUI may still be starting up on the foreground VT;
            // opening the card then could steal master from it.
            if matches!(is_graphics_mode(active), Ok(false)) {
                t.open_kms();
            }
            if let Some(img) = t.capture_current(active) {
                t.store(active, img);
            }
        }
        t
    }

    /// Opens the DRM card if it isn't already. Only done while a text
    /// console is in front (or a GUI is established), never right after we
    /// switched into a GUI — see the note at the top of `kms.rs`.
    fn open_kms(&mut self) -> bool {
        if self.kms.is_some() {
            return true;
        }
        if self.last_switch.elapsed() < GUI_SETTLE {
            return false;
        }
        match Kms::open() {
            Ok(k) => {
                self.kms = Some(k);
                true
            }
            Err(e) => {
                eprintln!("fourttyswipe: DRM unavailable, switches won't be animated: {e}");
                false
            }
        }
    }

    /// Called when four fingers touch down: capture the current screen and
    /// get buffers ready before the swipe is recognized.
    pub fn prepare(&mut self) {
        let Ok(vt) = self.vt.active_vt() else { return };
        if self.open_kms() {
            let fmt = self.fmt;
            if let Some(kms) = self.kms.as_mut() {
                if kms.refresh_output().is_ok() {
                    let _ = kms.ensure_buffers(fmt);
                }
            }
        }
        if let Some(img) = self.capture_current(vt) {
            self.store(vt, img.clone());
            self.prepared = Some(Prepared { vt, img, at: Instant::now() });
        }
        self.last_used = Instant::now();
    }

    /// Called periodically while idle.
    pub fn idle(&mut self) {
        if self.last_used.elapsed() >= IDLE_RELEASE {
            if let Some(kms) = self.kms.as_mut() {
                kms.release_buffers();
            }
        }
    }

    fn capture_current(&mut self, vt: u16) -> Option<Snapshot> {
        match is_graphics_mode(vt).ok()? {
            false => fb::capture().ok(),
            true => {
                if self.last_switch.elapsed() < GUI_SETTLE {
                    return None;
                }
                let fmt = self.fmt;
                let drm = match self.kms.as_mut() {
                    Some(k) => k.capture_scanout(fmt).map_err(|e| e.to_string()),
                    None => Err("DRM not open".into()),
                };
                match drm {
                    Ok(img) => Some(img),
                    Err(why) => {
                        let x = xcapture::capture(vt).map(|s| s.to_fmt(fmt));
                        if x.is_none() {
                            eprintln!("fourttyswipe: can't capture the GUI on VT{vt}: {why}");
                        }
                        x
                    }
                }
            }
        }
    }

    fn store(&mut self, vt: u16, img: Snapshot) {
        let gui = is_graphics_mode(vt).unwrap_or(true);
        if gui { &mut self.gui_cache } else { &mut self.tty_cache }.insert(vt, img);
    }

    /// The outgoing screen: from touch-down if recent, else captured now.
    fn outgoing(&mut self, vt: u16) -> Option<Snapshot> {
        match self.prepared.take() {
            Some(p) if p.vt == vt && p.at.elapsed() < PREPARED_MAX_AGE => Some(p.img),
            _ => {
                let img = self.capture_current(vt)?;
                self.store(vt, img.clone());
                Some(img)
            }
        }
    }

    pub fn swipe(&mut self, direction: SwipeDirection) {
        self.last_used = Instant::now();
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

        let started = Instant::now();
        let result = match (from_gui, to_gui) {
            (true, true) => Err("GUI to GUI switches aren't animated".into()),
            _ => self.animated(from, to, from_gui, to_gui, direction),
        };

        match result {
            Ok(t) => println!(
                "fourttyswipe: VT{from} -> VT{to} in {} ms, {} frames, {} late, worst frame {:.1} ms",
                started.elapsed().as_millis(),
                t.frames,
                t.late,
                t.worst.as_secs_f64() * 1000.0
            ),
            Err(why) => eprintln!("fourttyswipe: VT{from} -> VT{to} not animated: {why}"),
        }
        if self.vt.active_vt().ok() != Some(to) {
            if let Err(e) = self.vt.activate(to) {
                eprintln!("fourttyswipe: VT switch failed: {e}");
            }
        }
        if to_gui {
            self.last_switch = Instant::now();
        }
        self.last_used = Instant::now();
    }

    fn animated(&mut self, from: u16, to: u16, from_gui: bool, to_gui: bool, dir: SwipeDirection) -> Result<Timing> {
        let from_img = self
            .outgoing(from)
            .ok_or_else(|| format!("couldn't capture VT{from}"))?;
        let to_cached = if to_gui { self.gui_cache.get(&to) } else { self.tty_cache.get(&to) }.cloned();

        if !self.open_kms() {
            return Err("DRM not available".into());
        }
        let fmt = self.fmt;
        let kms = self.kms.as_mut().unwrap();
        kms.refresh_output().map_err(|e| format!("DRM: {e}"))?;
        kms.ensure_buffers(fmt).map_err(|e| format!("DRM buffers: {e}"))?;
        let size = kms.size().unwrap_or_default();
        let fits = |img: &Snapshot| (img.width, img.height) == size && img.fmt == fmt;
        if !fits(&from_img) {
            return Err(format!(
                "display is {}x{} but VT{from} capture is {}x{}",
                size.0, size.1, from_img.width, from_img.height
            ));
        }
        let to_cached = to_cached.filter(|img| fits(img));

        match (from_gui, to_gui) {
            (false, false) => self.text_to_text(from, to, &from_img, to_cached, dir),
            (false, true) => self.text_to_gui(from, to, &from_img, to_cached, dir),
            _ => self.gui_to_text(to, &from_img, to_cached, dir),
        }
    }

    fn text_to_text(&mut self, from: u16, to: u16, from_img: &Snapshot, cached: Option<Snapshot>, dir: SwipeDirection) -> Result<Timing> {
        let kms = self.kms.as_mut().unwrap();
        take_display(kms, from_img)?;

        // The screen is frozen on our copy of `from`; fbcon draws `to` into
        // its own (hidden) buffer during the switch.
        let result = (|| -> Result<Timing> {
            self.vt.activate(to).map_err(|e| format!("switch: {e}"))?;
            // A background console rarely changes, so its last capture lets
            // the slide start immediately; the handoff shows the live one.
            let to_img = match cached {
                Some(img) => img,
                None => fb::capture().map_err(|e| format!("capture VT{to}: {e}"))?,
            };
            if (to_img.width, to_img.height) != (from_img.width, from_img.height) {
                return Err("console resolutions differ".into());
            }
            Ok(animate(self.kms.as_mut().unwrap(), from_img, &to_img, dir))
        })();

        let fg = self.vt.active_vt().unwrap_or(from);
        handoff_to_text(self.kms.as_mut().unwrap(), fg);
        // fbcon just redrew the console for real; keep the cache current.
        if let Ok(img) = fb::capture() {
            self.tty_cache.insert(fg, img);
        }
        result
    }

    fn text_to_gui(&mut self, from: u16, to: u16, from_img: &Snapshot, cached: Option<Snapshot>, dir: SwipeDirection) -> Result<Timing> {
        let fmt = self.fmt;
        let to_img = match cached {
            Some(img) => img,
            // Never left this GUI while we were watching; ask X directly.
            None => xcapture::capture(to)
                .map(|s| s.to_fmt(fmt))
                .filter(|s| (s.width, s.height) == (from_img.width, from_img.height))
                .ok_or_else(|| format!("no screenshot of the GUI on VT{to} yet (it gets one the first time you swipe away from it)"))?,
        };
        self.gui_cache.insert(to, to_img.clone());

        let kms = self.kms.as_mut().unwrap();
        take_display(kms, from_img)?;
        let timing = animate(kms, from_img, &to_img, dir);

        // The GUI takes master when its VT activates; it must be free by then.
        kms.drop_master();
        if let Err(e) = self.vt.activate(to) {
            handoff_to_text(kms, from);
            return Err(format!("switch: {e}"));
        }
        // Our last frame stays up (no black gap) until the GUI shows its own.
        if !kms.wait_released(GUI_RECLAIM_TIMEOUT) {
            eprintln!("fourttyswipe: GUI on VT{to} hasn't put up its own frame after {:?}", GUI_RECLAIM_TIMEOUT);
        }
        Ok(timing)
    }

    fn gui_to_text(&mut self, to: u16, from_img: &Snapshot, cached: Option<Snapshot>, dir: SwipeDirection) -> Result<Timing> {
        let kms = self.kms.as_mut().unwrap();
        kms.draw_snapshot(from_img);

        match cached {
            Some(to_img) => {
                // Keeping the target in graphics mode through the switch
                // stops fbcon from reclaiming (and flashing) the display,
                // and grabbing master the moment the GUI releases it keeps
                // anything else from drawing. The GUI's last frame stays up
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
                    return Err(if got_master { "couldn't show our frame" } else { "GUI didn't release DRM master" }.into());
                }
                let timing = animate(kms, from_img, &to_img, dir);
                handoff_to_text(kms, to);
                if let Ok(img) = fb::capture() {
                    self.tty_cache.insert(to, img);
                }
                Ok(timing)
            }
            None => {
                // Without a prior image of this console, fbcon has to draw it
                // once before it can be captured, so it may flash for a frame.
                self.vt.activate(to).map_err(|e| format!("switch: {e}"))?;
                if !kms.acquire_master(MASTER_TIMEOUT) {
                    return Err("GUI didn't release DRM master".into());
                }
                if let Err(e) = kms.present() {
                    kms.drop_master();
                    return Err(format!("present: {e}"));
                }
                let to_img = match fb::capture() {
                    Ok(img) if (img.width, img.height) == (from_img.width, from_img.height) => img,
                    Ok(_) => {
                        handoff_to_text(kms, to);
                        return Err("console and GUI resolutions differ".into());
                    }
                    Err(e) => {
                        handoff_to_text(kms, to);
                        return Err(format!("capture VT{to}: {e}"));
                    }
                };
                self.tty_cache.insert(to, to_img.clone());
                let timing = animate(kms, from_img, &to_img, dir);
                handoff_to_text(kms, to);
                Ok(timing)
            }
        }
    }
}

/// From a text VT (nobody holds DRM master): show an identical copy of the
/// current screen from our own buffer.
fn take_display(kms: &mut Kms, img: &Snapshot) -> Result<()> {
    if !kms.acquire_master(MASTER_TIMEOUT) {
        return Err("another process is holding DRM master while a text console is showing \
                    (a GUI session on another VT that didn't release it?)"
            .into());
    }
    kms.draw_snapshot(img);
    if let Err(e) = kms.present() {
        kms.drop_master();
        return Err(format!("present: {e}"));
    }
    Ok(())
}

/// Gives the display back to fbcon on foreground text VT `vt`. Flipping the
/// VT graphics -> text makes the kernel force fbcon's framebuffer back onto
/// the CRTC and redraw, regardless of DRM master.
fn handoff_to_text(kms: &mut Kms, vt: u16) {
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

/// Real-time priority only while frames are being produced, so a busy
/// desktop can't make us miss vblanks — and so captures and other work at
/// touch-down don't compete with the desktop at elevated priority.
fn set_realtime(on: bool) {
    let (policy, prio) = if on { (libc::SCHED_FIFO, 20) } else { (libc::SCHED_OTHER, 0) };
    let param = libc::sched_param { sched_priority: prio };
    unsafe { libc::sched_setscheduler(0, policy, &param) };
}

fn animate(kms: &mut Kms, from: &Snapshot, to: &Snapshot, dir: SwipeDirection) -> Timing {
    set_realtime(true);
    let period = Duration::from_secs_f64(1.0 / kms.refresh_hz());
    let mut timing = Timing::default();
    let start = Instant::now();
    let mut last = start;
    loop {
        // One frame ahead: frame 0 (the untouched outgoing screen) is
        // already on display from the takeover.
        let t = ((start.elapsed() + period).as_secs_f64() / DURATION.as_secs_f64()).min(1.0);
        let eased = 1.0 - (1.0 - t).powi(3);
        compose(kms, from, to, dir, eased);
        if kms.present().is_err() {
            break;
        }
        let now = Instant::now();
        let interval = now - last;
        last = now;
        timing.frames += 1;
        timing.worst = timing.worst.max(interval);
        if interval > period.mul_f64(1.5) {
            timing.late += 1;
        }
        // Page flips pace us at vblank; if the driver fell back to
        // unsynchronized SETCRTC, pace manually instead of spinning.
        if interval < period / 4 {
            sleep(period - interval);
        }
        if t >= 1.0 {
            break;
        }
    }
    set_realtime(false);
    timing
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
