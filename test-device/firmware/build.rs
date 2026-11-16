//! Puts `memory.x` where the linker script `link.x` finds it, and passes
//! the build id (`TEST_DEVICE_BUILD`, set by `cargo xtask test-device
//! build`; "dev" otherwise) to the firmware's `hello`.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    fs::write(out.join("memory.x"), include_bytes!("memory.x")).expect("memory.x");
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-env-changed=TEST_DEVICE_BUILD");
    let build = env::var("TEST_DEVICE_BUILD").unwrap_or_else(|_| "dev".into());
    assert!(
        !build.is_empty()
            && build
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "TEST_DEVICE_BUILD must be one word of letters, digits and hyphens"
    );
    println!("cargo:rustc-env=TEST_DEVICE_BUILD={build}");
}
