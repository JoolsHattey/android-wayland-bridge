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

## Layout

- `native/`: the compositor (Rust, `wayland-server`), built as `libawb.so`.
- `app/`: the Kotlin layer (windows, insets, input).
- `scripts/`: deploy loop and chroot launcher.
- `tools/torture/`: a misbehaving client for robustness tests. Build it with
  `cargo build --release --target x86_64-unknown-linux-musl`.
