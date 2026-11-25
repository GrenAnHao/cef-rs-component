//! Standalone GPUI desktop browser example.
#[cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]
#[path = "../cef_example/mod.rs"]
mod cef_example;

fn main() {
    #[cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]
    {
        use cef_example::model::{ExampleConfig, STANDALONE_USAGE};
        std::process::exit(
            match ExampleConfig::parse_standalone_args(std::env::args_os().skip(1)) {
                Ok(Some(config)) => cef_example::run(config),
                Ok(None) => {
                    println!("{STANDALONE_USAGE}");
                    0
                }
                Err(err) => {
                    eprintln!("{err}\n{STANDALONE_USAGE}");
                    2
                }
            },
        );
    }
    #[cfg(not(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc")))]
    {
        eprintln!("gpui-cef-example requires Windows x64 MSVC");
        std::process::exit(2);
    }
}
