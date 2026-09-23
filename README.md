# fourttyswipe

Four-finger swipe between active TTYs — no keybind held, just the gesture.
Part of [HeroineOS](https://github.com/HeroineOS)'s
[fourswipe](https://github.com/HeroineOS/fourswipe-core) family.

## Why this is separate from the compositor

`fourttyswipe` runs as its own root-level system service, entirely outside
HeroiWM (or any compositor/WM). Two reasons that's required, not just
convenient:

1. **It has to work when there's no compositor on the target VT at all** —
   switching *to* a bare shell TTY, or *from* one, can't depend on a
   compositor being present to do the switching.
2. **It has to work regardless of whether a given session's compositor
   supports touch input.** `fourttyswipe` reads the touchscreen
   directly via `fourswipe-core`'s raw evdev backend
   (`/dev/input/eventN`), bypassing libinput/X11/Wayland input pipelines
   entirely. A GUI session with no touch support, a plain getty shell, and
   a touch-aware Wayland session are all switched between identically.

VT switching itself uses the same kernel mechanism as Ctrl+Alt+F&lt;N&gt;
(`VT_ACTIVATE`/`VT_WAITACTIVE` ioctls on `/dev/tty0`, see `src/vt.rs`) — it
doesn't care what's running on either end of the switch.

## Behavior

- Swipe left (4 fingers, across at least 40% of the screen) → next allocated VT
- Swipe right → previous allocated VT
- Cycles only through VTs the kernel reports as allocated (i.e. actually
  in use by a getty, display manager, or compositor), not blank ones.

## Slide transition

The switch is animated as a slide between screenshots of the two VTs. The
animation is shown from fourttyswipe's own DRM/KMS framebuffers with
vblank-synced page flips; fbcon's and the GUI's buffers are never written
to. The display is handed back to the real owner of the target VT on a
frame that matches what it's about to show.

| From → to | Outgoing image | Incoming image |
|---|---|---|
| tty → tty | fbdev | fbdev, captured after the switch while our frozen frame hides it |
| tty → X | fbdev | live X11 screenshot, or the last one taken if a backgrounded X server won't provide it |
| X → tty | X11 screenshot | last capture of that tty (so nothing flashes); first visit to a tty may flash one frame |
| X → X | — | not animated |

Wayland sessions aren't captured yet (needs a screencopy protocol in
HeroiWM), so switches involving them are instant.

## Requirements

- Root — needed for the VT ioctls, raw evdev reads, DRM master, and
  reading the X server's auth cookie.
- A touchscreen device that reports `ABS_MT_POSITION_X/Y`.
- For animation: a KMS display driver (`/dev/dri/cardN`) and a 32bpp or
  16bpp console; otherwise switches still work, just without animation.

## Running

```
cargo build --release
sudo ./target/release/fourttyswipe
```

Or install `fourttyswipe.service` for it to run system-wide, independent of
any login session:

```
sudo cp target/release/fourttyswipe /usr/bin/fourttyswipe
sudo cp fourttyswipe.service /etc/systemd/system/
sudo systemctl enable --now fourttyswipe
```

## Troubleshooting

When a switch isn't animated, the reason is logged:

```
journalctl -u fourttyswipe -f
```

## License

MIT OR Apache-2.0
