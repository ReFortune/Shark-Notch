//! Embeds `shark-notch.exe.manifest` (segment heap, supported OS) into the executable (MSVC linker
//! only). No resource compiler and no build dependency: the linker merges the XML itself.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default())
        .join("shark-notch.exe.manifest");
    println!("cargo:rerun-if-changed={}", manifest.display());

    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os == "windows" && env == "msvc" {
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
            manifest.display()
        );
    }
}
