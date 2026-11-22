#![cfg(target_os = "windows")]

mod support;

use std::{
    env, fs,
    io::Write,
    process::Command,
    thread,
    time::{Duration, Instant},
};

/// Child-side fixture for the bounded-wait tests. When the parent test runs this
/// binary normally the child-local environment flag is absent and this returns
/// immediately. When self-spawned with `--exact child_fixture`, the flag chooses a
/// plain Rust sleep or exit-37 path; PowerShell and AMSI are never involved.
#[test]
fn child_fixture() {
    let Some(mode) = env::var_os("GPUI_CEF_PROCESS_TIMEOUT_CHILD") else {
        return;
    };
    match mode.to_str() {
        Some("exit37") => {
            print!("child-fixture-stdout\n");
            eprint!("child-fixture-stderr\n");
            std::io::stdout().flush().unwrap();
            std::io::stderr().flush().unwrap();
            std::process::exit(37);
        }
        Some("sleep") => {
            print!("child-sleeping\n");
            std::io::stdout().flush().unwrap();
            std::io::stderr().flush().unwrap();
            thread::sleep(Duration::from_secs(30));
            std::process::exit(0);
        }
        other => panic!("unexpected child fixture mode: {other:?}"),
    }
}

fn self_spawn(mode: &str) -> Command {
    let mut command = self_spawn_without_mode();
    command.env("GPUI_CEF_PROCESS_TIMEOUT_CHILD", mode);
    command
}

fn self_spawn_without_mode() -> Command {
    let mut command = Command::new(env::current_exe().unwrap());
    command.args([
        "--exact",
        "child_fixture",
        "--nocapture",
        "--test-threads=1",
    ]);
    command.env_remove("GPUI_CEF_PROCESS_TIMEOUT_CHILD");
    command
}

#[test]
fn completed_child_returns_output_and_exit_code() {
    let started = Instant::now();
    let completed = support::run_bounded_with_log(
        self_spawn("exit37"),
        "self-spawn-exit37",
        Duration::from_secs(10),
    )
    .expect("the exit-37 child completes well before the deadline");
    let output = completed.output;
    assert_eq!(output.status.code(), Some(37));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("child-fixture-stdout"),
        "stdout read back from the owned log file: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("child-fixture-stderr"),
        "stderr read back from the owned log file: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "completed child must not wait for pipe EOF"
    );
    // A nonzero exit keeps its log directory for diagnosis, outside the fixture root.
    assert!(completed.log_directory.exists(), "exit-37 logs are kept");
    assert!(
        completed
            .log_directory
            .starts_with(support::bounded_log_root())
    );
}

#[test]
fn successful_child_removes_only_its_own_log_directory() {
    let completed = support::run_bounded_with_log(
        self_spawn_without_mode(),
        "self-spawn-success",
        Duration::from_secs(10),
    )
    .expect("the unflagged child fixture exits immediately");
    assert!(completed.output.status.success());
    assert!(
        !completed.log_directory.exists(),
        "success cleans only the run's own log directory: {:?}",
        completed.log_directory
    );
}

#[test]
fn blocked_child_hits_the_deadline_with_a_typed_timeout() {
    let limit = Duration::from_millis(800);
    let started = Instant::now();
    let error = support::run_bounded(self_spawn("sleep"), "self-spawn-sleep", limit)
        .expect_err("a 30-second child cannot finish inside an 800 ms deadline");
    let elapsed = started.elapsed();
    let timed_out = match error {
        support::BoundedCommandError::TimedOut(timed_out) => timed_out,
        other => panic!("expected BoundedCommandError::TimedOut, got {other:?}"),
    };
    assert_eq!(timed_out.limit, limit);
    assert!(
        elapsed >= limit,
        "returned before the deadline: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "deadline enforcement took too long: {elapsed:?}"
    );
    assert!(timed_out.log_directory.exists(), "timed-out logs are kept");
    assert!(
        String::from_utf8_lossy(&fs::read(&timed_out.stdout_log).unwrap())
            .contains("child-sleeping"),
        "stdout log retained for diagnosis"
    );
    assert!(
        timed_out.diagnostics.contains("only the owned process"),
        "diagnostics explain descendant scope: {}",
        timed_out.diagnostics
    );
}

#[test]
fn normal_child_fixture_is_immediate_without_the_child_flag() {
    let started = Instant::now();
    // This is the same body the self-spawned child runs, minus the child-local flag.
    child_fixture();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "child fixture must not sleep in the normal suite"
    );
    assert_eq!(
        env::var_os("GPUI_CEF_PROCESS_TIMEOUT_CHILD"),
        None,
        "the parent suite must not inherit the child-local flag"
    );
    assert!(
        support::bounded_log_root().ends_with("target/component-validation/packaging"),
        "failure logs survive Fixture::Drop: {:?}",
        support::bounded_log_root()
    );
}
