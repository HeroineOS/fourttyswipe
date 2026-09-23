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
            eprintln!(
                "tty-swipe: failed to open a touchscreen input device: {e}"
            );
            std::process::exit(1);
        }
    };

    let mut detector = GestureDetector::new(GestureConfig {
        required_fingers: 4,
        ..GestureConfig::default()
    });

    println!("tty-swipe: watching for four-finger swipes (left = next TTY, right = previous TTY)");

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
            // Best-effort: animates a slide-out on plain console VTs, silently
            // skipped on a GUI VT holding DRM master (see fb.rs/transition.rs).
            transition::slide_out_current_screen(direction);

            let forward = matches!(direction, SwipeDirection::Left);
            match switcher.switch(forward) {
                Ok(active_vt) => println!("tty-swipe: switched to VT{active_vt}"),
                Err(e) => eprintln!("tty-swipe: VT switch failed: {e}"),
            }
        }
    }
}
