use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=app.manifest");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os == "windows" && target_env == "msvc" {
        let manifest =
            PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("app.manifest");
        println!("cargo:rustc-link-arg-bin=rust-cef-helper=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bin=rust-cef-helper=/MANIFESTINPUT:{}",
            manifest.display()
        );
    }
}
