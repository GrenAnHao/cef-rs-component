#[cfg(target_os = "windows")]
mod browser;
#[cfg(target_os = "windows")]
mod ui;
#[cfg(target_os = "windows")]
mod windows;

fn main() {
    #[cfg(target_os = "windows")]
    std::process::exit(windows::run());

    #[cfg(not(target_os = "windows"))]
    {
        eprintln!(
            "This demo runs on Windows x64 MSVC. The std-only tests run with cargo test --lib."
        );
        std::process::exit(2);
    }
}
