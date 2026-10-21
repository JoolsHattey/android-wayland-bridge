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
   /run/awb/wayland-0                  protocol state, Vulkan compositing
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
| SurfaceView size change (user resize) | `xdg_toplevel.configure(w/scale, h/scale)`; the client's frame at the new size lands with Android's (see [Synced resizes](#synced-resizes); veiled resizing is off on the device) |
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

* Two buffer types: `wl_shm`, and `zwp_linux_dmabuf_v1` GPU buffers (see
  [GPU](#gpu)). A `wl_shm` buffer is **copied once** on commit into a
  compositor-owned texture (swizzled ARGB to RGBA) and released immediately.
  Client memory is never touched after the commit. That's the opposite of
  the failure mode in Google's bridge, where release bookkeeping crashed.
* **Damage.** Each commit's damage (`damage_buffer`, and `damage` mapped
  through the scale and viewport, rounded outwards) is kept as one bounding
  rect. Only that part is copied, over the surface's previous pixels. A
  commit that attaches a buffer without damage counts as all damaged, as
  does a new size or format. Textures remember the damage of their last 8
  commits, so the GPU path can catch up from any of them.
* Shrinking a pool under the compositor (SIGBUS) is caught the way
  libwayland does it: a SIGBUS handler, chained to ART's, maps zero pages
  over the pool and the client is disconnected.
* Each window is a `SurfaceView`. The compositor draws the toplevel tree
  (subsurfaces with sync/desync semantics, then popups) into its
  `ANativeWindow`, presents, then sends frame callbacks. Normally this runs
  through a Vulkan swapchain. The CPU fallback locks the window instead
  (RGBA_8888, window-sized) and posts it. Either way, swapchain or
  `unlockAndPost` back-pressure paces the clients.
* The renderer honours `wp_viewporter` (source crop and destination
  size). A buffer whose size matches its on-screen pixel rect is copied 1:1.
  That is what fractional-scale clients produce. Anything else, such as a
  2× or 1× buffer on a 1.5× window, is scaled bilinearly. Surface edges
  are rounded separately, so neighbouring surfaces meet without gaps.

### GPU

The compositor renders with Vulkan, and clients render on the GPU and pass
dmabufs. Both sides use the same Intel Arc (Meteor Lake) and `i915` render
node, `/dev/dri/renderD128`, which is mode 0666 and reaches the chroot
through its `/dev` bind mount.

* **Android side.** The vendor Vulkan driver is Mesa ANV (26.1). Android's
  GL is ANGLE on top of it, so EGL can't import dmabufs; Vulkan can. A probe
  on the device found `VK_EXT_external_memory_dma_buf`,
  `VK_EXT_image_drm_format_modifier` and `VK_EXT_queue_family_foreign`.
  ARGB/XRGB/ABGR/XBGR 8888 and 2101010 import with linear, X-tiled,
  4-tiled and MTL CCS modifiers: 40 format/modifier pairs.
* **Chroot side.** Debian's Mesa 25.0 needs `mesa-vulkan-drivers` and
  `libgl1-mesa-dri`. GTK then picks `GskGLRenderer` on the Arc,
  `es2gears_wayland` runs at the panel's 120 Hz, and `vkcube` uses ANV.
* **`zwp_linux_dmabuf_v1` v4** (`dmabuf.rs`). Feedback is for the render
  node's `dev_t`, in two tranches. The first holds the pairs SurfaceFlinger
  can show as is (8-bit RGB; linear, X- or 4-tiled; no CCS), so clients pick
  those and can go [direct](#direct-mode-zero-copy). The second holds the
  rest. Version 3 clients get `modifier` events. A buffer is imported into Vulkan when its `wl_buffer`
  is created, so a bad buffer fails there (`failed`, or `invalid_wl_buffer`
  for `create_immed`), never at draw time.
  - All planes must share one buffer object (as Mesa allocates CCS aux
    planes). The import uses the explicit plane layouts.
  - `y_invert` is honoured; interlaced buffers are rejected.
* **Renderer** (`gpu.rs`). One queue and one timeline semaphore; every
  submission signals the next serial.
  - **Per window:** a swapchain on the `ANativeWindow` (R8G8B8A8, FIFO). It
    lives in the window's slot, so `surfaceDestroyed` tears it down before
    the window is released.
  - **Pacing:** acquire never blocks the compositor thread. With
    `VK_GOOGLE_display_timing` (presents carry ids; past timings say which
    the display has shown), a window with 2 frames queued is not drawn until
    one is shown. The zero-timeout acquire stays as a backstop. Android logs
    every acquire that would block, so asking only when an image is free
    matters. A window whose display is slow then waits by itself; others keep
    their rate (two es2gears and a vkcube all hold 120 FPS).
  - **Per surface:** one quad, blended as premultiplied alpha, with a
    bilinear sampler. A quad that lands 1:1 on pixel centres samples
    exactly, so fractional-scale clients stay pixel-exact.
  - **Textures:** dmabuf images are acquired from and released back to
    `VK_QUEUE_FAMILY_FOREIGN_EXT` each frame. `wl_shm` textures are uploaded
    through a staging buffer when they change. An X-format swizzles alpha to
    1 in the image view.
* **Latching.** A window is drawn only once every buffer it shows is
  finished. A slow client then only delays its own window, instead of
  stalling the queue all windows share.
  - **Implicit sync:** the dmabuf polls readable.
  - **Explicit sync** (`syncobj.rs`, `wp_linux_drm_syncobj_v1`, offered
    when the render node has timeline syncobjs): each commit carries an
    acquire and a release point. Timelines are imported into our own DRM
    fd. The acquire point is checked with `SYNCOBJ_QUERY`; the release point
    is signalled from the CPU (`SYNCOBJ_TIMELINE_SIGNAL`) when the buffer
    would be released. A commit's release is a value that signals when
    dropped, so every path that lets go of a buffer gives it back, cached
    subsurface state included. Mesa's Vulkan WSI uses it (vkcube, vkgears
    at 120 FPS); GL clients stay on implicit sync. The protocol errors
    (`no_buffer`, `no_acquire_point`, `conflicting_points`, …) are checked
    at commit.
* **Buffer lifetime.** A dmabuf stays held while it is the surface's
  buffer. It is released when replaced, once the last frame that sampled it
  has completed. Images, staging buffers and readbacks are destroyed the
  same way. While frames are in flight, the event loop polls with a 2 ms
  timeout to collect them.
* **Shm uploads.** Each shm surface has a staging buffer that mirrors its
  texture. A commit copies only its damage since the staged commit into the
  staging buffer, and the next frame uploads only the part not yet in the
  image.
* **Header analysis.** After the render pass, the caption band is copied to
  a host buffer. When the frame completes, the same code as on the CPU path
  derives the control colour and gesture exclusion from it. A frame skips
  the readback when its band can't look different from the last one read:
  the same window size and caption, the same surfaces at the same places
  over the band, and none of their damage since then reaching the band's
  rows. Direct mode skips its readback the same way.
* **Fallback.** Without a usable Vulkan device, or with `renderer=cpu` in
  `files/awb.properties`, the CPU compositor runs and no dmabuf global is
  offered. Mesa then falls back to llvmpipe and GTK to cairo.
* Shaders (`native/shaders/`) are compiled to SPIR-V by `build.rs` with the
  NDK's `glslc`.

### Direct mode (zero-copy)

A window that is a single dmabuf surface, drawn 1:1 and covering the whole
window, is never blitted. Its buffer goes to SurfaceFlinger on a child
`SurfaceControl` of the window (`ASurfaceControl_createFromWindow`, z 1,
above the swapchain's layer), cropped to the window with
`ASurfaceTransaction_setGeometry`. (Windows that own their direct layer
get the buffer to set themselves instead; see
[Synced resizes](#synced-resizes).) This covers fractional-scale GTK windows,
whose buffer includes a resize margin around the window.

* **Wrapping the buffer** (`direct.rs`). There is no public way to make an
  AHardwareBuffer from a foreign dmabuf. `AHardwareBuffer_createFromHandle`
  exists, but returns `BAD_VALUE` here even for a genuine gralloc handle.
  The public `AHardwareBuffer_recvHandleFromUnixSocket` reads
  GraphicBuffer's flattened form (a `GB01` header, then the native
  handle's ints, with fds over `SCM_RIGHTS`). So we write that message
  ourselves around minigbm's `cros_gralloc_handle`, laid out as this
  device's gralloc does (native handle version 12, read from real buffers).
  - The handle carries one plane fd plus minigbm's metadata region. That
    region is a memfd filled from a template, which is read through
    `sendHandleToUnixSocket` from a fresh gralloc buffer. Without it,
    SurfaceFlinger's metadata queries log an error every frame.
  - The handle also carries strides, offsets, sizes, the DRM format,
    minigbm's i915 tiling for the modifier (0, 1 or 9), the modifier itself,
    use flags (`0x25`), the magic number, the HAL format (RGBA/RGBX/BGRA
    8888) and the usage (GPU sampled, framebuffer, composer overlay).
  - The mapper then imports the client's buffer object like any gralloc
    buffer. Each dmabuf is wrapped once, on first direct use.
* **Pacing.** One transaction in flight per window. Its `OnComplete` (on a
  binder thread) becomes `Cmd::DirectDone`, and the window's next frame
  waits for it.
* **Buffer release.** `OnComplete` also carries the previous buffer's
  release fence. A dmabuf counts its SurfaceFlinger holds, and is released
  (wl_buffer and syncobj point) only when both the GPU and every hold are
  done.
* **Leaving direct mode.** When a window needs compositing again (a popup,
  a resize the client has not caught up with, a scaled or second surface),
  the layer is hidden and given a 1×1 placeholder, so SurfaceFlinger hands
  the client's buffer back. A new Target means a new layer, and the old
  holds are dropped.
* **Header analysis.** The caption rows are copied straight out of the
  client's image, which is imported with `TRANSFER_SRC` where the modifier
  allows it. The analysis then runs as for a composited frame; exclusion
  works in direct mode.
* `direct=0` in `files/awb.properties` turns it off.

Verified on the device:

* libadwaita is direct: a 1260×924 4-tiled ARGB buffer for a 1200×864
  window.
* Opening the menu popover switches it to composited, and closing the
  popover switches it back to direct.
* vkcube at `scale=1` is direct with explicit sync and keeps animating.

### Synced resizes

Android resizes a window first and tells the app afterwards. Without help,
the compositor shows the client's old frame in the new bounds for a few
frames: content pinned top-left, clipped or stretched, jumping on a
left-edge drag. Wayland avoids this with `configure`/`ack_configure`; awb
carries that across to Android's own resize sync.

* `WaylandView` implements `SurfaceHolder.Callback2`. Android calls
  `surfaceRedrawNeededAsync` on every step of a resize (the shell's
  animated `resizeTask` and interactive edge drags alike), and holds the
  window's new bounds until `drawingFinished` runs, or about a second.
* The callback makes a blocking binder call, `redraw(win, w, h)`. It
  carries the size, so `surfaceChanged` sends no separate `resize`, which
  would reach the compositor first and be drawn unsynced.
* The compositor (`Cmd::RedrawSync`) configures the client if the logical
  size changed. It then shows nothing of that window until a commit
  acknowledges that configure (`ack_configure` applies with the next
  commit). Older frames are dropped, but their frame callbacks still fire,
  so the client keeps drawing.
* The window owns its direct layer (`ownsLayer`, right after `attach`):
  `WaylandView` makes `awb-direct` a child of its SurfaceView, and the
  compositor sends it frames instead of setting buffers itself. A frame is
  the client's buffer as a `HardwareBuffer`, its source crop, where it
  goes, whether it is opaque, and a token: `IToplevelHost.present` for
  ordinary frames, the `redraw` reply for a synced one. The window imports
  each buffer once (cached by id), sets it, waits for the present fence
  and reports `presented(win, token)`, which the compositor takes as
  `Cmd::DirectDone`. Windows of stubs built before this keep the
  compositor's own layer (and get no synced resizes).
* A synced frame does not carry the client's buffer. The window copies
  it, 1:1, into one of two buffers of its own (`HardwareBufferRenderer`,
  grown in 256 px steps, used in turn so a copy never goes into the one on
  screen), and applies that with its fence through `applyTransactionOnDraw`,
  so it joins the window's synced frame. Then `drawingFinished`. Bounds and
  content change in one SurfaceFlinger transaction.
  - Why the copy: Android 17's shell pairs the app's and the caption's
    transaction for every drag step (`DragResizingSynchronizer`). With a
    client buffer in the app's frame (the compositor's wrap of the client's
    dmabuf, see [Direct mode](#direct-mode-zero-copy)), that frame
    sometimes never committed, in roughly one fast drag in ten. The app and
    SystemUI's caption renderer then waited out `BLASTBufferQueue`'s
    one-second commit timeout on every step, and SystemUI hit an input
    ANR. Whose layer it was, whose listener was in the frame, and the
    window's launch frame made no difference. A buffer allocated by
    gralloc itself in its place never stalled.
  - Ordinary frames stay zero-copy: only resize steps pay for the copy.
* Fallbacks: past 100 ms (`SYNC_TIMEOUT`) without the client's frame, or
  for a composited frame (the swapchain is outside Android's sync), the
  window gets nothing back and the compositor draws as before. If the copy
  fails, the frame is applied on its own, outside the synced frame. If the
  window gave up waiting, the compositor sends the frame as an ordinary
  one. A frame that is never reported presented is dropped after a second.
* Not used: `SurfaceView.applyTransactionToFrame` (it waits for the
  SurfaceView's next queued buffer, which direct mode never sends), and
  applying the frame once the window's frame commits (safe, but a frame
  late, which shows).

Verified on the device (Adwaita Demo, edge drags injected with
`input mouse motionevent`, and by hand): every step's frame is handed over
in 5–20 ms and changes in the same SurfaceFlinger sample as the window's
bounds, against about 40 ms behind without sync. Screen recordings show no
stretched or clipped frames. Fast drags on freshly opened windows (the
worst case before the copy) ran 16 out of 16 without a stall.

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
   A window spanning the display's width is configured `maximized` (Android
   has no public "is maximised" API). GTK then styles it as
   `window.maximized`, and `gtk.css` carries a second set of rules under
   that selector from the last maximised caption, so each window matches
   its caption without a private protocol. GTK reads the file at start-up,
   so a geometry first seen later applies from the next app start.
5. **Control colour.** After each frame the compositor samples the header
   (on the GPU path, from the caption band it reads back)
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
strips. Headers with gradients or images become fully excluded, i.e.
click-only.

**Moves the app asks for** (`xdg_toplevel.move`, e.g. a drag on GTK's title
label, which the exclusion gives to the app) and `resize` go through a root
helper. `awb-launcher.sh` runs `dev.awb.TaskHelper` from the app's APK with
`app_process`. It reads `taskId left top right bottom` lines from the
`task` fifo and calls `IActivityTaskManager.resizeTask` (mode
`RESIZE_MODE_USER`). The window's own view drives it: on the request it
records the window bounds and the pressed pointer (display px). Each later
move of that pointer becomes new bounds sent through the session, and the
release still goes to the app. Bursts are coalesced to the latest line.
This is an emulated drag: no system snapping. Without the helper the
request is logged and ignored. Verified on the device: dragging GTK Demo's
title label and GNOME Console's (in its stub) moves the window with the
pointer.

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

## Clipboard and drag and drop

* **Clipboard.** `wl_data_device` is kept in step with Android's clipboard
  (`native/src/data.rs`).
  - A client's selection that offers text is read at once (off the
    compositor thread, 8 MiB cap) and put on Android's clipboard under the
    label "Linux app".
  - Only the focused app may read Android's clipboard, and stub windows run
    as other packages. So each window's activity hands the text over itself
    (`IToplevelSession.clipboard`), on focus and on clip changes. It does so
    once per clip (by timestamp) and never for our own label, since each
    read shows Android's "pasted" notice.
  - Android text is offered to Linux apps as the usual text types. Other
    types (images, files) stay on their side for now.
  - Verified: text copied in GTK Demo shows as dev.awb's clip; a URL copied
    in Chrome pastes into GTK Demo.
* **Drag and drop** runs as an Android drag, so it crosses windows (each its
  own Android window) and apps.
  - `start_drag` (only while a button is held on that window) reads the
    source's text, then the window starts `View.startDragAndDrop` with
    `DRAG_FLAG_GLOBAL`. The clip carries the text for Android apps, and
    the icon surface (shm only) becomes the shadow.
  - Every `WaylandView` forwards its drag events. Over our windows they
    become `wl_data_device` enter/motion/leave/drop with an offer of the
    Wayland source (clip label `dev.awb.drag`). Over Android apps they
    deliver the text.
  - Text dragged in from other apps is offered to the window under it and
    handed over on drop.
  - The source gets `dnd_drop_performed`/`dnd_finished`, or `cancelled`;
    the pointer belongs to the drag until Android ends it.
  - Verified: text dragged from GTK Demo's search field into GNOME Console
    (another stub app) is inserted there.

## IME

`zwp_text_input_v3` (`native/src/text_input.rs`) follows keyboard focus.
The enabled text input's content type, surrounding text (as UTF-16 offsets)
and cursor rectangle (window px) go to the window's view. There it makes
the view a text editor with a `TextInputConnection` (`window/.../TextInput.kt`).
The IME sees a virtual editor: the app's surrounding text with the composing
text at the cursor. Its edits come back as one `done` each:

* `commitText` and `finishComposingText` become `commit_string`.
* `setComposingText` becomes `preedit_string`.
* `deleteSurroundingText` becomes `delete_surrounding_text` (UTF-16 units to
  bytes through the surrounding text).
* `setComposingRegion` ending at the cursor deletes that text and resends it
  as preedit.
* Cursor moves become arrow keys.
* Editor actions become Enter.

Content purposes map to input types (password, number, URL, terminal
without suggestions, and so on), and the cursor rectangle is reported as
`CursorAnchorInfo` for floating candidate windows.

The window uses `SOFT_INPUT_ADJUST_NOTHING`. Apps place their own text
fields, and the relayout Android does for the IME otherwise stalls a
direct-mode window (frames stop, then it shows black).

Verified with GTK Demo: Android serves the field (`TextInputConnection`, text
input type, Done action) and keeps a direct-mode window updating. IME
commits are untested from adb: Gboard shows no soft keyboard while a
hardware keyboard is attached.

## Links

* **Out of the chroot.** `awb-run.sh` installs `/usr/local/bin/awb-open`
  as the chroot's default `http`/`https` handler (a desktop file plus
  `xdg-mime default`) and as `$BROWSER`. It writes each web link to the
  app's `open` fifo. `awb-launcher.sh` (root, Android side) reads the fifo
  and runs `am start -a VIEW -d <link>`, so links open in Chrome. Only
  `http(s)` links are passed on, and only as an intent's data.
* **Back in.** A stub declares a `VIEW`/`BROWSABLE` intent filter for each
  `x-scheme-handler/<scheme>` in its app's desktop file (`install-stub.sh`
  reads `MimeType`; the stub build generates the filter). A link in such a
  scheme reaches the stub's `LaunchActivity`, which calls
  `IToplevelSession.open(appId, uri)`. dev.awb checks the link (one word of
  printable ASCII) and writes `<id> <link>` to the `launch` fifo. The
  launcher then runs `gtk-launch <id> <link>`, so the app gets it as `%U`.
* **Claude Desktop** works this way. "Continue with Google" opens Chrome.
  "Continue to Claude" follows a `claude://` link back to the stub, and
  the running Claude gets it through Electron's single-instance handoff.
  Electron as root needs `--no-sandbox`, and there's no X server, so
  `/usr/local/bin/claude-desktop` in the chroot wraps it:
  `exec /usr/bin/claude-desktop --no-sandbox --ozone-platform=wayland --enable-features=WaylandWindowDecorations "$@"`.
  It comes before `/usr/bin` in the PATH, so the desktop file's `Exec` uses
  it.

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
* With the GPU renderer, `torture abort`, `popup` and `churn 40`, plus
  `kill -9` of vkcube, es2gears and libadwaita mid-frame (direct and
  composited), also leave the app running. No windows or `awb-direct`
  layers are left over.
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
* wl_shm and dmabuf rendering, composited with Vulkan (CPU fallback).
* GPU clients: GTK's GL renderer, EGL/GLES and Vulkan on the Arc.
* Per-window pacing, explicit sync (`wp_linux_drm_syncobj_v1`) and zero-copy
  direct mode for single-surface windows.
* Popups and tooltips.
* Keyboard and mouse input.
* Client cursors.
* libadwaita header bar in the transparent caption with the system controls
  on top.
* Light/dark control colour.
* Pixel-derived exclusion: click in the header, drag the window from empty
  header space; the app's own moves through the root helper.
* Clipboard (text) both ways, drag and drop across windows and apps, and
  the IME through `zwp_text_input_v3`.
* Per-window caption geometry for GTK (maximised windows).

Tested with adwaita-1-demo, gtk4-demo, foot, weston-simple-shm, vkcube and
es2gears_wayland.

## Known gaps and open questions

* **Scale.**
  - Integer-only clients are scaled bilinearly, which is slightly soft;
    fractional clients are pixel-exact.
  - Displays are laid out left to right by id, not by their real
    arrangement (which isn't public).
  - `gtk.css` sizes are per scale, so they are slightly off on the other
    display until GTK restarts.
* **Server-side decorations.** A client that asks for them
  (`zxdg_toplevel_decoration_v1.set_mode(server_side)`) gets them: its
  window keeps Android's caption opaque, Android lays the view out below
  it, and the client gets the whole surface (no caption band, no header
  analysis). Electron's framed windows ask for this because we offer
  xdg-decoration, which is how Claude's detached DevTools gets a title bar.
  Unset or client-side keeps the transparent caption.
* **Header height.** The GTK header (47 logical px) is taller than Android's
  caption (28 dp), so the system controls sit high in the header.
  `gtk.css` sets `min-height` but can't shrink below GTK's widgets.
* **Static CSS.** `gtk.css` is read at GTK start-up, so geometry first seen
  later (the first maximised window) applies from the next app start.
* **Chromium/Electron caption.** Electron's Linux title bar overlay
  (`titleBarOverlay`) draws its own caption buttons with fixed sizes and
  gives the page `env(titlebar-area-*)` clear of those alone. It ignores
  GTK's layout and theme on that path, and nothing tells it about Android's
  controls. On GNOME this is fine (Electron's buttons are the only ones);
  here the page's header runs under Android's controls. The proper fix is
  an inset hook in Electron (a full Electron build). For now
  `scripts/patch-claude-desktop.sh` patches Claude Desktop's `app.asar`
  with `scripts/claude-desktop-insets.js`:
  - it reads Android's caption insets from `/run/awb/caption.json` (logical
    px, which are dp; normal and maximised, written next to `gtk.css`);
  - it pads the page's title bar (`.dframe-chrome-bar`, whose inline
    padding comes from `env(titlebar-area-*)`) by those insets less the
    empty margin inside Android's rects (10 dp after the app chip, 52 dp
    before the first control), divided by the page zoom. The end goes
    through Claude's `--df-chrome-bar-end-gutter`;
  - it draws Electron's own buttons transparent, since they sit under
    Android's.

  The CSS hook is Claude-specific, and the script must be rerun after a
  Claude Desktop update (it keeps the original as `app.asar.awb-orig`).
  Claude refuses debugging switches on its command line, so the patch also
  opens DevTools itself when started with `AWB_DEVTOOLS=1`
  (`awb-run.sh env AWB_DEVTOOLS=1 /usr/local/bin/claude-desktop`).
  Verified on the device: the header's icons end before Android's controls
  at both edges.
* **Clipboard and drag types.** Text only; images and file lists stay on
  their side. A drag icon from a dmabuf (GL-rendered) has no shadow.
* **Popups** can't leave the window yet. Possible: a child window per popup,
  if freeform tasks allow it outside the task bounds (untested).
* **Window state.** No public API to maximise or fullscreen a task from the
  app; requests are acknowledged but ignored.
* **Launch mode.** A launch from a fullscreen task of ours inherits
  fullscreen (`desktop-first-but-fullscreen-cross-app-launch`). Avoid
  showing `MainActivity` while windows open; starting the service with
  `am start-foreground-service` sidesteps it.
* **Copies.** `wl_shm` clients still pay two CPU copies (client to texture,
  texture to staging), but only of their damage. Composited windows are one
  GPU blit per frame.
* **Readback.** The caption band (about 0.4 MB) is read back only when it
  may have changed. A client that redraws its header every frame (an
  animated header, or one that damages its whole buffer) still costs one
  readback per frame.
* **Direct mode** depends on minigbm's private handle layout and the
  GraphicBuffer wire format. A gralloc update could change either; the
  mapper then rejects the buffer (logged) and the window stays composited.
  Scaled buffers (integer-scale clients such as vkcube at 1.5×) and
  multi-surface windows (libdecor decorations, popups) still composite.
  SurfaceFlinger could scale them, but the header analysis needs window
  px.
* **Explicit sync** is CPU-side: the acquire point is polled, and release
  points are signalled after the GPU or SurfaceFlinger is done, not by a
  GPU-side signal.

## Roadmap

1. ~~Per-window scale, fractional scaling and viewporter.~~ Done.
2. ~~SSD mode (opaque system caption) per the client's decoration
   preference.~~ Done.
3. ~~`linux-dmabuf` with Vulkan import and compositing, per-window
   pacing, explicit sync, zero-copy direct mode.~~ Done.
4. ~~Dark mode and accent colour via the settings portal.~~ Done.
5. ~~Clipboard: `wl_data_device` ↔ `ClipboardManager` (text first), then
   drag and drop.~~ Done (text).
6. ~~IME: `zwp_text_input_v3` ↔ `InputConnection`.~~ Done.
7. ~~A per-window caption-geometry channel for GTK.~~ Done, through the
   maximised state. Window Controls Overlay hints for Chromium/Electron
   remain open (see Known gaps).
8. ~~Optional privileged drag helper for `xdg_toplevel.move` and
   `resize`.~~ Done.

Out of scope for now: X11 and XWayland. The target is native Wayland apps,
which covers GTK, Qt and Chromium/Electron (`--ozone-platform=wayland`).
