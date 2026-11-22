//! Type-check the real application modules on a Linux builder without CEF
//! binaries. This harness cannot link/run and does not validate Windows ABI,
//! native shaders, DLL staging, PowerShell, graphics or browser behavior.
#[path = "../../src/windows.rs"]
mod application;
#[path = "../../src/browser/mod.rs"]
mod browser;
#[path = "../../src/ui.rs"]
mod ui;

fn main() {
    let _entry_point: fn() -> i32 = application::run;
}
