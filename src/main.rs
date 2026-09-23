mod console_mode;
mod fb;
mod transition;
mod vt;

use std::sync::Arc;
use std::thread;

use fourswipe_core::backend::evdev_backend::EvdevBackend;
use fourswipe_core::backend::InputBackend;
use fourswipe_core::{GestureConfig, GestureDetector, GestureEvent, SwipeDirection};

use vt::VtSwitcher;

fn main() {
    let switcher = match VtSwitcher::open() {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("fourttyswipe: failed to open /dev/tty0 (run as root): {e}");
            std::process::exit(1);
        }
    };

    let mut backend = match EvdevBackend::open_default() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("fourttyswipe: failed to open a touchscreen input device: {e}");
            std::process::exit(1);
        }
    };

    let (screen_width, screen_height) = match backend.screen_extent() {
        Ok(extent) => extent,
        Err(e) => {
            eprintln!("fourttyswipe: failed to read touchscreen coordinate range: {e}");
            std::process::exit(1);
        }
    };

    let mut detector = GestureDetector::new(GestureConfig {
        required_fingers: 4,
        screen_width,
        screen_height,
        ..GestureConfig::default()
    });

    println!(
        "fourttyswipe: watching for four-finger swipes covering >=60% of the screen \
         (left = next TTY, right = previous TTY)"
    );

    loop {
        let event = match backend.next_event() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("fourttyswipe: input read error: {e}");
                continue;
            }
        };

        let Some(gesture) = detector.feed(event) else {
            continue;
        };

        if let GestureEvent::Recognized { finger_count: 4, direction } = gesture {
            // Hard-reset immediately: the switch + transition below can take
            // hundreds of ms, during which the kernel keeps queuing touch
            // events from the tail end of this same physical swipe (finger
            // lift-off, settle jitter). Without this, that backlog gets
            // replayed into the detector once we're back to reading events
            // and can leave it in a state that requires a second full swipe
            // to shake loose. Discard it all now instead of trying to make
            // sense of it later.
            detector.cancel();

            // Capture (if sane) before switching, off the hot input-read path.
            let outgoing_vt = switcher.active_vt().ok();
            let outgoing_is_graphics = outgoing_vt
                .and_then(|vt| console_mode::is_graphics_mode(vt).ok())
                .unwrap_or(true); // unknown -> assume graphics, skip safely
            let outgoing = if outgoing_is_graphics {
                None
            } else {
                transition::capture_outgoing()
            };

            let switcher = Arc::clone(&switcher);
            let forward = matches!(direction, SwipeDirection::Left);

            // Run the switch + animation on its own thread so the main loop
            // keeps reading touch input the whole time — see the cancel()
            // comment above for why that matters.
            thread::spawn(move || match switcher.switch(forward) {
                Ok(active_vt) => {
                    println!("fourttyswipe: switched to VT{active_vt}");
                    let incoming_is_graphics =
                        console_mode::is_graphics_mode(active_vt).unwrap_or(true);
                    if let (Some((fb, snapshot)), false) = (outgoing, incoming_is_graphics) {
                        transition::slide_in(&fb, &snapshot, direction);
                    }
                }
                Err(e) => eprintln!("fourttyswipe: VT switch failed: {e}"),
            });
        }
    }
}
