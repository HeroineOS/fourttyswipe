//! "Curtain slide" transition: animates the *outgoing* console's captured
//! frame sliding off-screen, then the real VT switch happens and the
//! incoming session is revealed live underneath — no capture of the
//! incoming side is needed. Only meaningful on a plain fbcon VT (see
//! `fb.rs` for why); on a GUI VT this is a graceful no-op fallback to an
//! instant switch.

use std::thread::sleep;
use std::time::{Duration, Instant};

use fourswipe_core::SwipeDirection;

use crate::fb::Framebuffer;

const DURATION: Duration = Duration::from_millis(220);
const TARGET_FPS: u32 = 60;

/// Attempts a slide-out animation of the currently displayed console. Best
/// effort: any failure (no `/dev/fb0`, GUI session holding DRM master,
/// unexpected geometry) is swallowed and just skips the animation — the
/// caller still performs the real VT switch either way.
pub fn slide_out_current_screen(direction: SwipeDirection) {
    let fb = match Framebuffer::open() {
        Ok(fb) => fb,
        Err(_) => return,
    };

    let bytes_per_pixel = (fb.geometry.bits_per_pixel / 8).max(1) as usize;
    if bytes_per_pixel == 0 || fb.geometry.line_length == 0 || fb.geometry.width == 0 {
        return;
    }

    let snapshot = match fb.capture() {
        Ok(s) => s,
        Err(_) => return,
    };

    let horizontal = matches!(direction, SwipeDirection::Left | SwipeDirection::Right);
    let row_bytes = fb.geometry.line_length as usize;
    let height = fb.geometry.height as usize;
    let width_px = fb.geometry.width as usize;

    if snapshot.len() < row_bytes * height {
        return; // geometry didn't match what we captured; bail out safely
    }

    let start = Instant::now();
    let frame_interval = Duration::from_secs_f64(1.0 / TARGET_FPS as f64);

    loop {
        let elapsed = start.elapsed();
        if elapsed >= DURATION {
            break;
        }
        let t = elapsed.as_secs_f64() / DURATION.as_secs_f64();
        // Ease-out cubic: starts fast, settles smoothly rather than a linear slide.
        let eased = 1.0 - (1.0 - t).powi(3);

        let mut frame = vec![0u8; snapshot.len()];

        if horizontal {
            let shift_px = (eased * width_px as f64) as usize;
            let sign_left = matches!(direction, SwipeDirection::Left);
            for row in 0..height {
                let row_start = row * row_bytes;
                let src_row = &snapshot[row_start..row_start + row_bytes];
                let dst_row = &mut frame[row_start..row_start + row_bytes];
                if shift_px >= width_px {
                    continue; // fully off-screen, row stays black
                }
                let visible_px = width_px - shift_px;
                let visible_bytes = visible_px * bytes_per_pixel;
                if sign_left {
                    // content moves left: visible part is the tail of the row,
                    // written starting at column 0
                    let src_off = shift_px * bytes_per_pixel;
                    dst_row[..visible_bytes].copy_from_slice(&src_row[src_off..src_off + visible_bytes]);
                } else {
                    // content moves right: visible part is the head of the row,
                    // written starting at the shifted column
                    let dst_off = shift_px * bytes_per_pixel;
                    dst_row[dst_off..dst_off + visible_bytes].copy_from_slice(&src_row[..visible_bytes]);
                }
            }
        } else {
            let shift_rows = (eased * height as f64) as usize;
            let sign_up = matches!(direction, SwipeDirection::Up);
            if shift_rows < height {
                let visible_rows = height - shift_rows;
                if sign_up {
                    let src_off = shift_rows * row_bytes;
                    frame[..visible_rows * row_bytes]
                        .copy_from_slice(&snapshot[src_off..src_off + visible_rows * row_bytes]);
                } else {
                    let dst_off = shift_rows * row_bytes;
                    frame[dst_off..dst_off + visible_rows * row_bytes]
                        .copy_from_slice(&snapshot[..visible_rows * row_bytes]);
                }
            }
        }

        if fb.blit(&frame).is_err() {
            return; // e.g. lost the framebuffer mid-animation; abandon cleanly
        }

        sleep(frame_interval);
    }
}
