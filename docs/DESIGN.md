# Android Wayland Bridge: design

An Android app that runs a Wayland compositor and shows every Linux
`xdg_toplevel` as its own Android desktop window. Apps draw their own header
bar into Android's transparent caption, with the system's window controls on
top.

Target: gbOS (Android 17 desktop, x86_64) with Linux apps in a Debian chroot.
Validated on a Dell Inspiron 14 Plus 7440 running build CL3B.260622.271.R1.

## Why not Google's bridge

The Terminal app (`com.android.virtualization.terminal`) has a compositor,
`wayland_android_bridge`. It isn't open source and it runs inside the VM host
app. It panics with `subtract with overflow` in
`WlHostSurface::on_swapchain_image_released` when a client such as
weston-simple-shm exits, which kills the Terminal app, all of its windows and
the VM. It also draws server-side captions, so apps can't use the title bar.

This project is a separate app with its own process. A client can only ever
disconnect itself.

## Architecture

```
 Debian chroot (root)                Android app dev.awb (one process)
 ─────────────────────               ───────────────────────────────────────────
 GTK / foot / Electron ──wayland──▶  compositor thread (Rust, libawb.so)
   WAYLAND_DISPLAY=                    wayland-server (pure-Rust backend)
   /run/awb/wayland-0                  protocol state, CPU compositing
 (bind mount of the app's                    │ JNI callbacks (map, title, …)
  files/ directory)                          ▼
                                     main thread (Kotlin)
                                       AwbApp: one ToplevelActivity per toplevel
                                       ToplevelActivity: own task, freeform window,
                                         transparent caption, insets, exclusion
                                       WaylandView (SurfaceView): pixels + input
```

### Tech choice: Rust and `wayland-server`, with a thin Kotlin layer

* **Rust**: the protocol layer parses untrusted input from other processes.
  Memory safety plus `catch_unwind` around every request handler lets us drop
  one client instead of the whole app.
* **Not Smithay (yet).** Smithay 0.7 hard-depends on the `xkbcommon` crate,
  which links `libxkbcommon`. Android has no libxkbcommon, and linking the
  test build failed with `ld.lld: unable to find library -lxkbcommon`. We'd
  have to cross-build libxkbcommon with meson, bison and the NDK for little
  gain. A compositor only needs to *send* a keymap; clients compile it with
  their own libxkbcommon. So we use `wayland-server` (Smithay's own protocol
  crate, pure-Rust backend, no libwayland) and implement the protocols
  ourselves: about 2k lines so far. Smithay pieces (positioner math, a
  renderer) can be vendored later if it pays off.
* **Kotlin, no AndroidX.** Only a few framework classes are involved
  (Activity, SurfaceView, WindowInsets), so the APK stays small.
* Build: Gradle 9.5 / AGP 9.3 (built-in Kotlin). A `cargoBuild` task runs
  `cargo ndk -t x86_64 --platform 35` into `app/build/rustJniLibs`.
  NDK r30, minSdk 35, target/compileSdk 37.

### Threading

* **Compositor thread** (`server::run`). It owns all Wayland state. It
  `poll()`s on the listening socket, the display fd and an eventfd. Each
  iteration it: accepts clients, drains Android commands, dispatches
  clients, renders dirty windows, then flushes.
* **UI thread.** JNI entry points (`Native.*`) push `Cmd`s onto a mutex queue
  and poke the eventfd; they never block on the compositor. The one exception
  is `attachSurface(null)` in `surfaceDestroyed`: it takes the window's slot
  lock, so it waits for an in-flight frame and the compositor never draws
  into a released `ANativeWindow`.
* **Callbacks** (map, unmap, title, minimise, cursor, caption brightness,
  exclusion rects) run on the compositor thread, which stays attached to the
  JVM. They only `post` to the main looper.

### Window mapping

| Wayland | Android |
|---|---|
| `xdg_toplevel` gets its first buffer | `startActivity(ToplevelActivity)`: `NEW_TASK \| MULTIPLE_TASK \| NEW_DOCUMENT`, `ActivityOptions.setLaunchBounds` = client size |
| `set_title` | `Activity.title` and `TaskDescription` label |
| SurfaceView size change (user resize) | `xdg_toplevel.configure(w/scale, h/scale)`; client redraws live (veiled resizing is off on the device) |
| window focus | keyboard enter/leave and the `activated` state |
| Android close (✕) | `xdg_toplevel.close`; the client decides |
| toplevel destroyed / unmapped / client gone | `finishAndRemoveTask()` |
| `set_minimized` | `moveTaskToBack(true)` |
| `set_maximized` / `set_fullscreen` | acknowledged with a configure only; no app API (see open questions) |
| `xdg_popup` | composited into the parent's window and constrained to it (flip, slide, resize) |
| `wl_pointer.set_cursor` | `PointerIcon.create(bitmap, hotspot)` on the view |

* Configures always carry `tiled_{left,right,top,bottom}`, so GTK drops its
  shadows and rounded corners and the content fills the Android window.
  Android rounds the corners itself.
* `ToplevelActivity` declares the full `configChanges` set, so desktop
  resizing never recreates it.
* Tasks outlive the process. A `ToplevelActivity` restored after a restart
  has no client and finishes itself.

### Buffers to screen

* `wl_shm` only, for now. On commit the buffer is **copied once** into a
  compositor-owned texture (swizzled ARGB to RGBA) and released immediately.
  Client memory is never touched after the commit. That's the opposite of
  the failure mode in Google's bridge, where release bookkeeping crashed.
* Shrinking a pool under the compositor (SIGBUS) is caught the way
  libwayland does it: a SIGBUS handler, chained to ART's, maps zero pages
  over the pool and the client is disconnected.
* Each window is a `SurfaceView`. The compositor locks its `ANativeWindow`
  (RGBA_8888, window-sized), composites the toplevel tree (subsurfaces with
  sync/desync semantics, then popups) on the CPU, posts the frame, then sends
  frame callbacks. `unlockAndPost` back-pressure paces the clients.
* The renderer honours `wp_viewporter` (source crop and destination
  size). A buffer whose size matches its on-screen pixel rect is copied 1:1.
  That is what fractional-scale clients produce. Anything else, such as a
  2× or 1× buffer on a 1.5× window, is scaled bilinearly. Surface edges
  are rounded separately, so neighbouring surfaces meet without gaps.

### Scale, per display

* Kotlin reports every Android display through `DisplayManager` and its
  listener: mode size, density, refresh rate, physical size and name. Each
  display becomes its own `wl_output` global, with `xdg_output` logical
  geometry. Displays are laid out left to right by id.
* A display's scale is its Android density, kept in 120ths as
  `wp_fractional_scale_v1` does: 240 dpi gives 180 (1.5×) and the 223 dpi
  4K monitor gives 167 (1.39×). One GTK logical px is therefore one Android
  dp, as with Chrome's CSS px. `scale=<float>` in `files/awb.properties`
  overrides it for all displays.
* Each window follows its Activity's display. `ToplevelActivity` reports it
  in `onCreate` (before the surface exists) and again on configuration
  changes, which is how a move between displays arrives. Window space is in
  logical px; configure sizes are `floor(px / scale)`.
* `output::ensure_scale` runs for the toplevel before every configure, for
  popups before their first configure, and for every surface the renderer
  paints. It sends what changed: `wl_surface.leave/enter` for the outputs,
  `preferred_buffer_scale` (scale rounded up, for integer-only clients) and
  `wp_fractional_scale_v1.preferred_scale`.
* New windows open on the display of the last focused window (Kotlin passes
  `setLaunchDisplayId`, and the compositor uses the same display as its
  default). Clients therefore render the first frame at the right scale.
  The compositor sends logical sizes to `onMap`; Kotlin multiplies by the
  target display's density for the launch bounds.
* Cursor images are scaled by the window's scale over their buffer scale
  before becoming a `PointerIcon`.
* Verified on the device:
  - GTK binds both protocols.
  - It gets `preferred_scale(180)` on the panel and renders 1260×924 for
    840×616 logical.
  - After `moveToNextDisplay`, the window becomes 1115 px wide (still 800
    logical) at `preferred_scale(167)`.
  - A new gtk4-demo window on the 4K display renders 1169×891 for an
    840×640 viewport.

### Input

* Mouse and touch come from `WaylandView`. Hover goes through
  `onGenericMotionEvent`; pressed pointers come through `onTouchEvent`.
  Button bits map to `BTN_LEFT/RIGHT/MIDDLE/SIDE/EXTRA`. Injected events and
  touch have no button bits, so a pressed pointer counts as the primary
  button. There is an implicit grab while a button is held. A press outside
  grabbing popups sends `popup_done` and is consumed.
* Scroll maps to `axis` + `axis_discrete`, 15 units per detent.
* Keys use `KeyEvent.scanCode`, which is the Linux evdev code on real
  keyboards. Synthetic events fall back to a keycode table. Android's key
  repeats are dropped (clients repeat themselves per
  `wl_keyboard.repeat_info`). Modifiers are tracked in the compositor and
  sent as xkb masks for the bundled `us`/`pc105` keymap (compiled with
  `xkbcli` and shipped as `native/src/keymap_us.xkb`).

### How the socket reaches the chroot

The compositor listens on `/data/user/10/dev.awb/files/wayland-0` (mode 0777;
SELinux is permissive on the target). `scripts/awb-run.sh` runs as root on
the Android side. It bind-mounts that directory into the chroot at
`/run/awb`, then runs the command via `/data/debian/enter` with
`WAYLAND_DISPLAY=/run/awb/wayland-0`. The Terminal app's socket is left alone.

## Headers in the title bar

1. `ToplevelActivity` sets `APPEARANCE_TRANSPARENT_CAPTION_BAR_BACKGROUND`.
   Under the desktop edge-to-edge rules the SurfaceView already covers the
   caption, so the client's header shows through. `customizable_window_headers`
   is enabled on the device.
2. Every toplevel is told `zxdg_toplevel_decoration_v1.mode = client_side`
   and `wm_capabilities = []`.
3. `WindowInsets.getBoundingRects(captionBar())` gives the system's rects:
   the app chip on the left (112 px) and the window controls on the right
   (275 px) on this device. A maximised window also has a full-width
   status-bar rect, and its caption starts below it. The compositor then
   offsets the surface and pointer by that `top` and configures the client
   that much shorter.
4. **GTK.** The app writes `files/gtk.css` from those rects.
   `awb-run.sh` sets `gtk-decoration-layout=close:close` and imports the CSS.
   A real button is needed on both sides: with `menu:` the start box is
   hidden and its `min-width` reserves nothing.
   GTK's own controls stay in the layout but are invisible and sized to the
   system rects (`windowcontrols.start/end { min-width }`). Libadwaita's split
   header bars therefore leave exactly the right room at the outer edges, and
   only there. This works because those positions are covered by the
   system's input region anyway.
5. **Control colour.** After each frame the compositor samples the header
   under the controls and toggles `APPEARANCE_LIGHT_CAPTION_BARS` (dark icons
   on light headers).
6. **Chromium/Electron** (not built yet): run with
   `--enable-features=WaylandWindowDecorations` plus Window Controls Overlay.
   Chromium needs the overlay rect; plan: a small private protocol or env
   hint, see the roadmap.

### The drag question (answered)

**Can an app start an Android window drag programmatically?** Not with public
API. The SDK 37 stubs have nothing for moving or dragging a task. The only
related public API is `WindowManager.transferTouchGesture(InputTransferToken,
InputTransferToken)`, which needs a token for the system caption's input
window that apps can't get. Task bounds can be changed with `am task resize`
/ `IActivityTaskManager.resizeTask`, but that needs `MANAGE_ACTIVITY_TASKS`,
a signature permission, so only shell or root.

**How the system caption shares input** (measured on the device; WMShell
logs `relayout … touchableRegion` and `handleFreeformMotionEvent`):

* WMShell puts an input window over the whole caption strip. Its touchable
  region is the caption *minus* the app's `View.setSystemGestureExclusionRects`
  (it listens through `ISystemGestureExclusionListener`).
* **No exclusion:** WMShell takes every press in the caption. A drag moves
  the window natively, with snapping. A click goes nowhere: DOWN/UP were
  consumed (`ACTION_UP was not dragging, ignore`), so header buttons are dead.
* **Whole caption excluded:** every click reaches the app. GTK then calls
  `xdg_toplevel.move`, which we can't honour.

**What we do: pixel-derived exclusion** (`exclusion=auto`, the default). The
compositor already composites the header, so after each frame it scans the
caption band. A column that is one uniform colour from top to bottom of the
band is empty header. Everything else (icons, labels, entries, hovered
buttons) becomes an exclusion span, dilated by 6 px and merged across gaps
under 12 px. The spans go to `setSystemGestureExclusionRects` only when they
change. Results on the device with Adwaita Demo:

* WMShell's touchable region became `(0,0,112,56)(279,0,333,56)(375,0,1600,56)`,
  so the drawn widgets were carved out.
* A click on the menu button reached GTK and opened the popover.
* A drag on empty header space moved the window natively, from (1216,432) to
  (1016,672).

It needs no client co-operation, so it also covers Qt and Chromium tab
strips. Limits: a press on a label or title that GTK would treat as a drag
goes to the app and is lost (the `move` request is logged and ignored).
Headers with gradients or images become fully excluded, i.e. click-only.

Fallbacks for `xdg_toplevel.move` if needed, both privileged:

* (a) A root helper started by `awb-run.sh` that calls
  `IActivityTaskManager.resizeTask` as the pointer moves. This is an emulated
  drag with no system snapping.
* (b) An AccessibilityService that dispatches a gesture into the
  now-unexcluded caption.

Settings for experiments, in `files/awb.properties`:
`exclusion=auto|caption|none`.

## Appearance: dark mode and accent colour

On a Linux desktop, the shell (GNOME Shell, KWin) is the source of the
appearance settings. Apps read them from `org.freedesktop.portal.Settings`
on the session bus and follow `SettingChanged` live. Here the compositor
takes that role (`native/src/portal.rs`):

* `awb-run.sh` starts one session `dbus-daemon` for the chroot's apps. The
  start is serialised with util-linux `flock -o` so concurrent launches
  don't start two. Its socket lives in the app's files dir: `/run/awb/bus`
  inside the chroot, `files/bus` for the compositor. Apps get
  `DBUS_SESSION_BUS_ADDRESS` pointing at it.
* The compositor connects from Android (thread `awb-portal`) and owns
  `org.freedesktop.portal.Desktop`. If the daemon restarts it reconnects
  (polling every second, with a liveness check every 5 s).
  - dbus-daemon only admits uids it can resolve. `awb-run.sh` therefore
    registers the app's uid (owner of its files dir) as the system user
    `awb` in the chroot's passwd and group files.
  - The bus policy allows only `root` and `awb`.
* `org.freedesktop.appearance` is served with `ReadAll`, `ReadOne` and the
  legacy `Read`:
  - `color-scheme` is 1 (dark) or 2 (light), from
    `Configuration.isNightModeActive`.
  - `accent-color` is Material You's `system_accent1_500` as `(ddd)`.
* `AwbApp` publishes both on start and in `onConfigurationChanged`, so a
  night-mode toggle or a wallpaper change reaches running apps as
  `SettingChanged`.
* GTK looks for the portal once, at start-up. On a fresh bus,
  `awb-run.sh` waits up to about 3 s for the name before starting the app.
* Verified: libadwaita apps switch light/dark live with
  `cmd uimode night yes|no`, and their header changes flip Android's
  caption icons via the existing brightness check.
  - Plain GTK 4.18 apps (no libadwaita) stay light, as on GNOME. GTK 4.20
    maps the portal's scheme to `gtk-interface-color-scheme`.
  - GTK3/Qt aren't covered yet.
* With a session bus, GApplication apps also behave as on a desktop: they
  can be single-instance and are activatable.

## Robustness

* Every request handler runs in `guard()` (`catch_unwind`). A panic logs and
  kills only the offending client. Commands, dispatch and rendering are
  wrapped as well. `panic = "unwind"` in all profiles.
* Client data is validated before use. Pool and buffer bounds are checked
  in i64. Region op lists, surface-tree depth and parent chains are bounded,
  and subsurface cycles are rejected.
* Cleanup never depends on destruction order. On disconnect, objects die in
  any order, so windows and popups are found by their own resource. The
  torture test found exactly this bug: windows leaked when the `xdg_surface`
  died before its `xdg_toplevel`.
* `tools/torture` is a static musl client that misbehaves on purpose:
  `abort` (dies mid-frame with a commit in flight), `sigbus` (truncates its
  pool under an attached buffer), `popup` (exits with a popup open) and
  `churn N` (maps and destroys windows rapidly). All of these, plus
  weston-simple-shm exiting (the Google bridge crash) and `kill -9` of
  gtk4-demo, leave the app running with no leftover windows.

## Status (prototype, 2026-10-21)

Working on the device:

* Each toplevel is its own freeform Android window, with title,
  resize→configure and close.
* wl_shm rendering.
* Popups and tooltips.
* Keyboard and mouse input.
* Client cursors.
* libadwaita header bar in the transparent caption with the system controls
  on top.
* Light/dark control colour.
* Pixel-derived exclusion: click in the header, drag the window from empty
  header space.

Tested with adwaita-1-demo, gtk4-demo, foot and weston-simple-shm.

## Known gaps and open questions

* **Scale.**
  - Integer-only clients are scaled bilinearly, which is slightly soft;
    fractional clients are pixel-exact.
  - Displays are laid out left to right by id, not by their real
    arrangement (which isn't public).
  - `gtk.css` sizes are per scale, so they are slightly off on the other
    display until GTK restarts.
* **SSD-preferring clients** (foot) still draw their own title bar, a thin
  strip with an extra ✕. Planned: honour
  `zxdg_toplevel_decoration_v1.set_mode(server_side)` by keeping Android's
  caption opaque and placing the surface below it.
* **Header height.** The GTK header (47 logical px) is taller than Android's
  caption (28 dp), so the system controls sit high in the header.
  `gtk.css` sets `min-height` but can't shrink below GTK's widgets.
* **Static CSS.** `gtk.css` is written per device and read at GTK start-up,
  so it is wrong for maximised windows (narrower rects). A per-window
  channel would fix it: a private protocol, or a GTK module that reads
  `xdg_toplevel` configure hints.
* **Popups** can't leave the window yet. Possible: a child window per popup,
  if freeform tasks allow it outside the task bounds (untested).
* **Window state.** No public API to maximise or fullscreen a task from the
  app; requests are acknowledged but ignored.
* **Launch mode.** A launch from a fullscreen task of ours inherits
  fullscreen (`desktop-first-but-fullscreen-cross-app-launch`). Avoid
  showing `MainActivity` while windows open; starting the service with
  `am start-foreground-service` sidesteps it.
* **CPU path.** About 20 MB copied per full-screen frame, and frame pacing
  is by `unlockAndPost` back-pressure on the single compositor thread. A
  slow window delays the others.

## Roadmap

1. ~~Per-window scale, fractional scaling and viewporter.~~ Done.
2. SSD mode (opaque system caption) per the client's decoration preference.
3. `linux-dmabuf`: import client dmabufs as `AHardwareBuffer` and present
   them through `ASurfaceTransaction` (`SurfaceControl`) without copies.
   This also lets each window pace on its own transactions.
4. ~~Dark mode and accent colour via the settings portal.~~ Done.
5. Clipboard: `wl_data_device` ↔ `ClipboardManager` (text first), then
   drag and drop.
6. IME: `zwp_text_input_v3` ↔ `InputConnection`.
7. Window Controls Overlay hints for Chromium/Electron; a per-window
   caption-geometry channel for GTK.
8. Optional privileged drag helper for `xdg_toplevel.move` and
   `resize`.
