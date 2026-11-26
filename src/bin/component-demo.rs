//! Explicit, opt-in root demo for the public CefBrowser component.
#[cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]
#[path = "../cef_example/mod.rs"]
mod cef_example;
#[cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]
#[path = "../component_demo/cli.rs"]
mod cli;
#[cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]
#[path = "../component_demo/host.rs"]
mod host;
#[cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]
#[path = "../component_demo/shell.rs"]
mod shell;

fn main() {
    #[cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]
    std::process::exit(match cli::parse(std::env::args_os().skip(1)) {
        Ok(Some(config)) => host::run(config),
        Ok(None) => {
            println!("{}", cli::USAGE);
            0
        }
        Err(e) => {
            eprintln!("{e}\n{}", cli::USAGE);
            2
        }
    });
    #[cfg(not(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc")))]
    {
        eprintln!("gpui-cef-component-demo requires Windows x64 MSVC");
        std::process::exit(2);
    }
}
