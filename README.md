# tty-swipe

Four-finger swipe between active TTYs — no keybind held, just the gesture.
Part of [HeroineOS](https://github.com/HeroineOS)'s
[fourswipe](https://github.com/HeroineOS/fourswipe-core) family.

## Why this is separate from the compositor

`tty-swipe` runs as its own root-level system service, entirely outside
HeroiWM (or any compositor/WM). Two reasons that's required, not just
convenient:

1. **It has to work when there's no compositor on the target VT at all** —
   switching *to* a bare shell TTY, or *from* one, can't depend on a
   compositor being present to do the switching.
2. **It has to work regardless of whether a given session's compositor
   supports touch input.** `tty-swipe` reads the touchscreen
   directly via `fourswipe-core`'s raw evdev backend
   (`/dev/input/eventN`), bypassing libinput/X11/Wayland input pipelines
   entirely. A GUI session with no touch support, a plain getty shell, and
   a touch-aware Wayland session are all switched between identically.

VT switching itself uses the same kernel mechanism as Ctrl+Alt+F&lt;N&gt;
(`VT_ACTIVATE`/`VT_WAITACTIVE` ioctls on `/dev/tty0`, see `src/vt.rs`) — it
doesn't care what's running on either end of the switch.

## Behavior

- Swipe left (4 fingers) → next allocated VT
- Swipe right (4 fingers) → previous allocated VT
- Cycles only through VTs the kernel reports as allocated (i.e. actually
  in use by a getty, display manager, or compositor), not blank ones.

## Requirements

- Root (or `CAP_SYS_TTY_CONFIG` + `/dev/input` access) — needed for both
  the VT ioctls and raw evdev reads.
- A touchscreen device that reports `ABS_MT_POSITION_X/Y`.

## Running

```
cargo build --release
sudo ./target/release/tty-swipe
```

Or install `tty-swipe.service` for it to run system-wide, independent of
any login session:

```
sudo cp target/release/tty-swipe /usr/bin/tty-swipe
sudo cp tty-swipe.service /etc/systemd/system/
sudo systemctl enable --now tty-swipe
```

## Status

Builds and links against `fourswipe-core`; VT-switching and evdev-reading
logic is implemented but not yet validated on real hardware.

## License

MIT OR Apache-2.0
