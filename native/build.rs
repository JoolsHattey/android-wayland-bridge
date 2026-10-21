//! Compiles shaders/*.{vert,frag} to SPIR-V with the NDK's glslc.

use std::path::{Path, PathBuf};
use std::process::Command;

fn glslc() -> PathBuf {
    let exe = if cfg!(windows) { "glslc.exe" } else { "glslc" };
    let mut ndks: Vec<PathBuf> = ["ANDROID_NDK_HOME", "ANDROID_NDK_ROOT", "ANDROID_NDK"]
        .iter()
        .filter_map(|v| std::env::var_os(v).map(PathBuf::from))
        .collect();
    for sdk in ["ANDROID_HOME", "ANDROID_SDK_ROOT"].iter().filter_map(|v| std::env::var_os(v)) {
        if let Ok(dirs) = std::fs::read_dir(Path::new(&sdk).join("ndk")) {
            let mut v: Vec<PathBuf> = dirs.flatten().map(|d| d.path()).collect();
            v.sort();
            ndks.extend(v.into_iter().rev());
        }
    }
    for ndk in ndks {
        if let Ok(hosts) = std::fs::read_dir(ndk.join("shader-tools")) {
            for h in hosts.flatten() {
                let p = h.path().join(exe);
                if p.exists() {
                    return p;
                }
            }
        }
    }
    PathBuf::from(exe)
}

fn main() {
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let glslc = glslc();
    for name in ["quad.vert", "quad.frag"] {
        let src = Path::new("shaders").join(name);
        println!("cargo:rerun-if-changed={}", src.display());
        let status = Command::new(&glslc)
            .args(["-O", "--target-env=vulkan1.1", "-o"])
            .arg(out.join(format!("{name}.spv")))
            .arg(&src)
            .status()
            .unwrap_or_else(|e| panic!("cannot run {} (set ANDROID_NDK_HOME): {e}", glslc.display()));
        assert!(status.success(), "glslc failed on {name}");
    }
}
