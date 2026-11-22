// Standalone argv/exit-code regression; invoked by test-cargo-run.ps1.
use std::{
    env,
    process::{Command, ExitCode},
};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--probe") {
        println!("{:?}", &args[1..]);
        return ExitCode::from(37);
    }
    let exe = env::current_exe().expect("probe path");
    let runner = env::current_dir()
        .expect("project root")
        .join("scripts/cargo-run.ps1");
    let passed = [
        "plain",
        "a b",
        "",
        "a\"b",
        "tail\\",
        "&",
        "%PATH%",
        "--nocapture",
    ];
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(runner)
        .arg(exe)
        .arg("--probe")
        .args(passed)
        // Non-package tools (including export-cef-dir) must start without CEF.
        .env("CEF_PATH", "nonexistent-cef-for-runner-test")
        .output()
        .expect("run runner");
    assert_eq!(
        output.status.code(),
        Some(37),
        "child exit code: {output:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("{passed:?}"),
        "child arguments"
    );
    println!("Cargo runner argv/exit-code regression passed.");
    ExitCode::SUCCESS
}
