# Android Wayland Bridge

A Wayland compositor as an Android app. Each Linux toplevel opens as its own
Android desktop window, and apps draw their header bar into Android's
transparent caption. See [docs/DESIGN.md](docs/DESIGN.md).

## Build and run

Requirements: Android SDK (platform 37), NDK r30, Rust with
`x86_64-linux-android`, and `cargo-ndk`.

```bash
./scripts/deploy.sh
```

`deploy.sh` builds the APK and installs it. It then restarts the compositor
service and pushes the chroot launcher to the device. `ADB_SERIAL` selects
the device.

On the device, as root, run any Wayland app from the Debian chroot:

```bash
adb shell /data/local/tmp/awb-run.sh adwaita-1-demo
```

To give a Linux app its own launcher icon, build and install its stub app
(name and icon come from the chroot's desktop file):

```bash
./scripts/install-stub.sh org.gnome.Adwaita1.Demo
```

Or let `deploy.sh` do it: it runs `./scripts/sync-stubs.sh`, which builds
stubs for the apps that opened windows without one (and have a desktop
file), rebuilds stubs whose desktop file or icon changed, and removes those
whose app is gone (the root launcher also removes these on its own within
two minutes of an `apt remove`). Stubs carry no window code: they run
dev.awb's, so updating dev.awb updates every stub, with nothing to rebuild.
See [Stub apps](docs/DESIGN.md#stub-apps).

The icon starts the app through `awb-launcher.sh` (the root side, started by
`deploy.sh`, which also runs the helper that moves windows when an app asks
to, as on a title-label drag). The app's windows then open as the stub's tasks, so the
taskbar and recents show them under that app.

Links cross over both ways. Web links that a Linux app opens (`xdg-open`,
`$BROWSER`) open in Android's browser. Links in an app's own URL schemes
(the `x-scheme-handler` types in its desktop file, such as `claude://`)
open in the app, through its stub. That is how a browser sign-in returns
to the app.

For GPU rendering in the apps, the chroot needs Mesa's drivers
(`apt install mesa-vulkan-drivers libgl1-mesa-dri`). The compositor itself
composites with Vulkan; `renderer=cpu` in the app's `files/awb.properties`
switches back to the CPU compositor.

Games and 3D apps can lock the pointer (`zwp_pointer_constraints_v1`):
the window then takes Android's pointer capture, which ends when it loses
focus (Meta or Alt+Tab); a click back into the window locks it again.
Stub apps built before stubs ran dev.awb's code (format 1) lack pointer
lock, touchpad scrolling and full-rate motion until `sync-stubs.sh` (run by
`deploy.sh`) replaces them.

Electron apps lay their title bar out around Electron's own buttons, not
Android's. For Claude Desktop, `./scripts/patch-claude-desktop.sh` patches
its `app.asar` to keep clear of Android's caption controls; rerun it after
Claude Desktop updates.

## Layout

- `native/`: the compositor (Rust, `wayland-server`), built as `libawb.so`.
- `app/`: the Kotlin layer: compositor callbacks, displays, the window
  session service.
- `window/`: the window activity (insets, input, surface), shared by `app/`
  and the stubs; it talks to the compositor over binder.
- `stub/`: the app of one Linux app (launcher icon, window tasks); its
  activities' code is loaded from dev.awb at run time.
- `scripts/`: deploy loop, chroot and app launchers, stub installer and
  sync (`awb-stubs.sh` is their device side).
- `tools/torture/`: a misbehaving client for robustness tests. Build it with
  `cargo build --release --target x86_64-unknown-linux-musl`.
