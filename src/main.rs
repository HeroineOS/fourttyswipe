mod fb;
mod transition;
mod vt;

use fourswipe_core::backend::evdev_backend::EvdevBackend;
use fourswipe_core::backend::InputBackend;
use fourswipe_core::{GestureConfig, GestureDetector, GestureEvent, SwipeDirection};

fn main() {
    let switcher = match vt::VtSwitcher::open() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("tty-swipe: failed to open /dev/tty0 (run as root): {e}");
            std::process::exit(1);
        }
    };

    let mut backend = match EvdevBackend::open_default() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("tty-swipe: failed to open a touchscreen input device: {e}");
            std::process::exit(1);
        }
    };

    let (screen_width, screen_height) = match backend.screen_extent() {
        Ok(extent) => extent,
        Err(e) => {
            eprintln!("tty-swipe: failed to read touchscreen coordinate range: {e}");
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
        "tty-swipe: watching for four-finger swipes covering >=60% of the screen \
         (left = next TTY, right = previous TTY)"
    );

    loop {
        let event = match backend.next_event() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("tty-swipe: input read error: {e}");
                continue;
            }
        };

        let Some(gesture) = detector.feed(event) else {
            continue;
        };

        if let GestureEvent::Recognized { finger_count: 4, direction } = gesture {
            // Best-effort: only works between plain console VTs (see
            // fb.rs/transition.rs); silently skipped otherwise, in which
            // case the switch below still happens, just without the animation.
            let outgoing = transition::capture_outgoing();

            let forward = matches!(direction, SwipeDirection::Left);
            match switcher.switch(forward) {
                Ok(active_vt) => {
                    println!("tty-swipe: switched to VT{active_vt}");
                    if let Some((fb, snapshot)) = outgoing {
                        transition::slide_in(&fb, &snapshot, direction);
                    }
                }
                Err(e) => eprintln!("tty-swipe: VT switch failed: {e}"),
            }
        }
    }
}
