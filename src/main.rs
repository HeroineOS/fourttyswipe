mod console_mode;
mod fb;
mod kms;
mod snapshot;
mod transition;
mod vt;
mod xcapture;

use std::sync::mpsc;
use std::thread;

use fourswipe_core::backend::evdev_backend::EvdevBackend;
use fourswipe_core::backend::InputBackend;
use fourswipe_core::{GestureConfig, GestureDetector, GestureEvent, SwipeDirection};

use transition::Transitioner;
use vt::VtSwitcher;

fn main() {
    let switcher = match VtSwitcher::open() {
        Ok(s) => s,
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

    let config = GestureConfig {
        required_fingers: 4,
        screen_width,
        screen_height,
        ..GestureConfig::default()
    };
    let mut detector = GestureDetector::new(config);

    // One worker runs transitions strictly one at a time, so a quick second
    // swipe queues behind the first instead of drawing over it. Input keeps
    // being read on this thread the whole time.
    let (tx, rx) = mpsc::channel::<SwipeDirection>();
    thread::spawn(move || {
        let mut transitioner = Transitioner::new(switcher);
        for direction in rx {
            transitioner.swipe(direction);
        }
    });

    println!(
        "fourttyswipe: watching for four-finger swipes covering >={:.0}% of the screen \
         (left = next TTY, right = previous TTY)",
        config.recognize_fraction * 100.0
    );

    loop {
        let event = match backend.next_event() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("fourttyswipe: input read error: {e}");
                continue;
            }
        };

        let Some(GestureEvent::Recognized { finger_count: 4, direction }) = detector.feed(event) else {
            continue;
        };

        // Discard the rest of this physical swipe (lift-off, jitter) so it
        // can't be read as the start of another gesture.
        detector.cancel();

        if matches!(direction, SwipeDirection::Left | SwipeDirection::Right) {
            let _ = tx.send(direction);
        }
    }
}
