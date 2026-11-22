// Test-only Cargo fixture for test-cargo-run-wait.py; never a browser smoke test.
use std::{env, fs, process, thread, time::Duration};

fn main() {
    let root =
        std::path::PathBuf::from(env::var_os("G0_CARGO_WAIT_DIR").expect("fixture directory"));
    if env::args().nth(1).as_deref() == Some("--linger") {
        thread::sleep(Duration::from_secs(30));
        return;
    }

    assert_eq!(
        env::args().skip(1).collect::<Vec<_>>(),
        [
            "run",
            "--locked",
            "--no-default-features",
            "--features",
            "legacy-demo",
            "--bin",
            "gpui-cef-poc"
        ]
    );
    fs::write(root.join("parent.pid"), process::id().to_string()).unwrap();
    match env::var("G0_CARGO_WAIT_MODE").as_deref() {
        Ok("timeout") => thread::sleep(Duration::from_secs(30)),
        Ok("failure") => process::exit(37),
        _ => {
            let child = process::Command::new(env::current_exe().unwrap())
                .arg("--linger")
                .stdin(process::Stdio::null())
                .stdout(process::Stdio::null())
                .stderr(process::Stdio::null())
                .spawn()
                .unwrap();
            fs::write(root.join("child.pid"), child.id().to_string()).unwrap();
            println!("FAKE_CARGO_FIXTURE: validates the script, not CEF rendering");
            println!("First CEF frame: 2x2 BGRA");
            eprintln!("CEF browser closed; shutdown complete. Frames received: 1");
        }
    }
}
