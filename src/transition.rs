//! "Slide over" transition: captures the outgoing screen, performs the real
//! VT switch, briefly waits for the incoming console to settle, captures
//! it too, then animates a slide between the two static screenshots (the
//! new one entering from the swipe direction as the old one exits). Once
//! the animation finishes on the final frame, the real (already-active)
//! incoming VT is left showing, matching exactly — the animation frame and
//! the live content are the same content, seamless handoff.
//!
//! Only meaningful when both ends are plain fbcon VTs (see `fb.rs`); if
//! capture fails on either end, this is skipped and the switch already
//! happened instantly underneath, so there's no user-visible fallback
//! state to handle here.

use std::thread::sleep;
use std::time::{Duration, Instant};

use fourswipe_core::SwipeDirection;

use crate::fb::Framebuffer;

const DURATION: Duration = Duration::from_millis(260);
const TARGET_FPS: u32 = 60;
/// Give the incoming VT a moment to actually draw before we snapshot it
/// (a getty/shell repaints near-instantly, but isn't literally synchronous
/// with the VT_WAITACTIVE ioctl returning).
const SETTLE_DELAY: Duration = Duration::from_millis(60);

/// Captures the currently displayed screen, if this is a plain console VT.
/// Call this *before* switching.
pub fn capture_outgoing() -> Option<(Framebuffer, Vec<u8>)> {
    let fb = Framebuffer::open().ok()?;
    let snapshot = fb.capture().ok()?;
    Some((fb, snapshot))
}

/// Call this *after* the real VT switch has completed. Captures the new
/// screen (once settled) and animates the slide from `outgoing` to it.
pub fn slide_in(fb: &Framebuffer, outgoing: &[u8], direction: SwipeDirection) {
    sleep(SETTLE_DELAY);

    let incoming = match fb.capture() {
        Ok(s) => s,
        Err(_) => return,
    };

    let bytes_per_pixel = (fb.geometry.bits_per_pixel / 8).max(1) as usize;
    let row_bytes = fb.geometry.line_length as usize;
    let height = fb.geometry.height as usize;
    let width_px = fb.geometry.width as usize;

    if bytes_per_pixel == 0 || row_bytes == 0 || width_px == 0 {
        return;
    }
    if outgoing.len() != incoming.len() || outgoing.len() < row_bytes * height {
        // Geometry changed between the two captures (e.g. the incoming VT
        // is a different resolution) — nothing sane to slide between.
        return;
    }

    let horizontal = matches!(direction, SwipeDirection::Left | SwipeDirection::Right);
    let start = Instant::now();
    let frame_interval = Duration::from_secs_f64(1.0 / TARGET_FPS as f64);

    loop {
        let elapsed = start.elapsed();
        if elapsed >= DURATION {
            break;
        }
        let t = elapsed.as_secs_f64() / DURATION.as_secs_f64();
        let eased = 1.0 - (1.0 - t).powi(3); // ease-out cubic

        let mut frame = vec![0u8; outgoing.len()];

        if horizontal {
            let offset_px = ((eased * width_px as f64) as usize).min(width_px);
            let offset_bytes = offset_px * bytes_per_pixel;
            let leaving = matches!(direction, SwipeDirection::Left);
            for row in 0..height {
                let row_start = row * row_bytes;
                let dst_row = &mut frame[row_start..row_start + row_bytes];
                let out_row = &outgoing[row_start..row_start + row_bytes];
                let in_row = &incoming[row_start..row_start + row_bytes];

                if leaving {
                    // Outgoing exits to the left, incoming enters from the right.
                    let visible_out = width_px - offset_px;
                    let visible_out_bytes = visible_out * bytes_per_pixel;
                    dst_row[..visible_out_bytes]
                        .copy_from_slice(&out_row[offset_bytes..offset_bytes + visible_out_bytes]);
                    dst_row[visible_out_bytes..]
                        .copy_from_slice(&in_row[..row_bytes - visible_out_bytes]);
                } else {
                    // Outgoing exits to the right, incoming enters from the left.
                    dst_row[..offset_bytes].copy_from_slice(&in_row[row_bytes - offset_bytes..]);
                    dst_row[offset_bytes..]
                        .copy_from_slice(&out_row[..row_bytes - offset_bytes]);
                }
            }
        } else {
            let offset_rows = ((eased * height as f64) as usize).min(height);
            let leaving_up = matches!(direction, SwipeDirection::Up);
            if leaving_up {
                let visible_out = height - offset_rows;
                frame[..visible_out * row_bytes]
                    .copy_from_slice(&outgoing[offset_rows * row_bytes..]);
                frame[visible_out * row_bytes..]
                    .copy_from_slice(&incoming[..offset_rows * row_bytes]);
            } else {
                frame[..offset_rows * row_bytes]
                    .copy_from_slice(&incoming[(height - offset_rows) * row_bytes..]);
                frame[offset_rows * row_bytes..]
                    .copy_from_slice(&outgoing[..(height - offset_rows) * row_bytes]);
            }
        }

        if fb.blit(&frame).is_err() {
            return;
        }
        sleep(frame_interval);
    }

    // Leave the real, already-live incoming frame showing (identical to
    // where the animation ended, so this is a no-op visually).
    let _ = fb.blit(&incoming);
}
