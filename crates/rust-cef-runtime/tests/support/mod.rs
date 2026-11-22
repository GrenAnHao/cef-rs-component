#![allow(dead_code)]
use std::{
    fs,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use rust_cef_core::contracts::RuntimeFiles;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const STAMP: &str = "GPUI-CEF-COMPONENT|helper|schema=1|target=x86_64-pc-windows-msvc|component=0.1.0|cef=156.0.31|binding=7b6bf84e76e919790bd61df077e29dc4132eb5e2";
const RUNTIME_PATHS: &[&str] = &[
    "libcef.dll",
    "chrome_elf.dll",
    "d3dcompiler_47.dll",
    "dxcompiler.dll",
    "vulkan-1.dll",
    "vk_swiftshader.dll",
    "vk_swiftshader_icd.json",
    "chrome_100_percent.pak",
    "chrome_200_percent.pak",
    "resources.pak",
    "icudtl.dat",
    "v8_context_snapshot.bin",
    "archive.json",
    "locales/en-US.pak",
];
static NEXT: AtomicU64 = AtomicU64::new(1);

/// Metadata-only PE fixtures, never passed to the operating system for execution.
pub struct Fixture {
    pub root: PathBuf,
    pub host: PathBuf,
    pub helper: PathBuf,
    pub manifest: Value,
}

pub fn pe(machine: u16, helper: bool) -> Vec<u8> {
    let mut bytes = vec![0; 256];
    bytes[..2].copy_from_slice(b"MZ");
    bytes[0x3c..0x40].copy_from_slice(&128u32.to_le_bytes());
    bytes[128..132].copy_from_slice(b"PE\0\0");
    bytes[132..134].copy_from_slice(&machine.to_le_bytes());
    if helper {
        bytes.extend_from_slice(STAMP.as_bytes());
    }
    bytes
}

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

impl Fixture {
    pub fn new() -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/component-file-fixtures")
            .join(format!(
                "部署 fixture {}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(root.join("locales")).unwrap();
        let root = root.canonicalize().unwrap();
        let host = root.join("my host.exe");
        let helper = root.join("CEF child.exe");
        let host_bytes = pe(0x8664, false);
        let helper_bytes = pe(0x8664, true);
        fs::write(&host, &host_bytes).unwrap();
        fs::write(&helper, &helper_bytes).unwrap();
        let mut runtime = Vec::new();
        for path in RUNTIME_PATHS {
            let bytes = if path.ends_with(".dll") {
                pe(0x8664, false)
            } else if *path == "archive.json" {
                br#"{"type":"minimal","name":"cef_binary_156.0.31+fixture+chromium-156.0.8078.100_windows64_minimal.tar.bz2","sha1":"fixture"}"#.to_vec()
            } else {
                b"metadata fixture, not CEF runtime bytes".to_vec()
            };
            fs::write(root.join(path), &bytes).unwrap();
            runtime.push(json!({"path":path,"sha256":digest(&bytes)}));
        }
        let manifest = json!({
            "schema":1,"target":"x86_64-pc-windows-msvc","cef_version":"156.0.31",
            "binding_revision":"7b6bf84e76e919790bd61df077e29dc4132eb5e2",
            "component_version":"0.1.0",
            "host":{"file_name":"my host.exe","build_version":"test-host","sha256":digest(&host_bytes)},
            "helper":{"file_name":"CEF child.exe","build_version":"0.1.0","sha256":digest(&helper_bytes)},
            "runtime":runtime
        });
        let value = Self {
            root,
            host,
            helper,
            manifest,
        };
        value.save();
        value
    }

    pub fn files(&self) -> RuntimeFiles {
        RuntimeFiles {
            directory: self.root.clone(),
            helper_executable: self.helper.clone(),
        }
    }

    pub fn save(&self) {
        fs::write(
            self.root.join("component-runtime.json"),
            serde_json::to_vec_pretty(&self.manifest).unwrap(),
        )
        .unwrap();
    }

    pub fn replace_helper(&mut self, bytes: &[u8]) {
        fs::write(&self.helper, bytes).unwrap();
        self.manifest["helper"]["sha256"] = json!(digest(bytes));
        self.save();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

// ---------------------------------------------------------------------------
// Bounded child-process execution for test harnesses.
//
// `Command::output()` waits for the child *and* for pipe EOF. Windows PowerShell
// (and compiler/csc descendants it starts) can hand inherited pipe handles to
// long-lived grandchildren, so EOF may never arrive even after the process we
// care about is gone. Instead, this helper redirects stdout/stderr to log files
// owned by this run, polls `Child::try_wait` up to a deadline, and only kills the
// exact `Child` it spawned. Descendants are not enumerated or terminated.
// AMSI/Defender are never disabled or bypassed here.
// ---------------------------------------------------------------------------

/// Default hard limit for one PowerShell invocation from these tests.
pub const DEFAULT_PS_TIMEOUT: Duration = Duration::from_secs(60);

/// Directory where bounded-command logs are retained on failure.
///
/// Deliberately outside `Fixture::root`: `Fixture::Drop` deletes that tree, and
/// timeout diagnostics must survive fixture teardown.
pub fn bounded_log_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/component-validation/packaging")
}

static NEXT_LOG: AtomicU64 = AtomicU64::new(1);

fn new_log_directory(label: &str) -> std::io::Result<PathBuf> {
    let sanitized: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let directory = bounded_log_root().join(format!(
        "{}-{}-{}",
        sanitized,
        std::process::id(),
        NEXT_LOG.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&directory)?;
    Ok(directory)
}

/// Why a bounded child invocation did not produce a normal `Output`.
#[derive(Debug)]
pub enum BoundedCommandError {
    /// The child was still running at the deadline. Only the owned child was
    /// killed and reaped; descendants may still be alive.
    TimedOut(TimedOut),
    /// The child could not be started; logs (possibly empty) are retained.
    Spawn {
        source: std::io::Error,
        log_directory: PathBuf,
    },
    /// Log files could not be created or read back.
    Io {
        source: std::io::Error,
        path: PathBuf,
        log_directory: PathBuf,
    },
}

impl BoundedCommandError {
    pub fn log_directory(&self) -> &std::path::Path {
        match self {
            Self::TimedOut(timed_out) => &timed_out.log_directory,
            Self::Spawn { log_directory, .. } | Self::Io { log_directory, .. } => log_directory,
        }
    }
}

impl std::fmt::Display for BoundedCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimedOut(timed_out) => write!(
                f,
                "TimedOut after {:?} (limit {:?}, child pid {}): {}; logs: {}",
                timed_out.waited,
                timed_out.limit,
                timed_out.child_id,
                timed_out.diagnostics,
                timed_out.log_directory.display()
            ),
            Self::Spawn {
                source,
                log_directory,
            } => write!(
                f,
                "failed to spawn child: {source}; logs: {}",
                log_directory.display()
            ),
            Self::Io {
                source,
                path,
                log_directory,
            } => write!(
                f,
                "bounded-command I/O failed for {}: {source}; logs: {}",
                path.display(),
                log_directory.display()
            ),
        }
    }
}

impl std::error::Error for BoundedCommandError {}

/// Typed outcome of hitting the deadline.
#[derive(Debug)]
pub struct TimedOut {
    pub limit: Duration,
    pub waited: Duration,
    pub child_id: u32,
    pub log_directory: PathBuf,
    pub stdout_log: PathBuf,
    pub stderr_log: PathBuf,
    /// `None` when the owned child could not be reaped after `kill`.
    pub exit: Option<std::process::ExitStatus>,
    /// Human-readable scope note, retained in the failure output.
    pub diagnostics: String,
}

/// A completed bounded command plus the log path it owned.
#[derive(Debug)]
pub struct BoundedOutput {
    pub output: std::process::Output,
    /// Removed only when the child exited successfully.
    pub log_directory: PathBuf,
}

/// Run `command` bounded, returning `std::process::Output` so existing
/// assertions keep working. Successful exits remove only this run's log
/// directory; timeouts, spawn errors and nonzero exits keep it for diagnosis.
pub fn run_bounded(
    command: Command,
    label: &str,
    limit: Duration,
) -> Result<std::process::Output, BoundedCommandError> {
    run_bounded_with_log(command, label, limit).map(|completed| completed.output)
}

pub fn run_bounded_with_log(
    mut command: Command,
    label: &str,
    limit: Duration,
) -> Result<BoundedOutput, BoundedCommandError> {
    let log_directory = new_log_directory(label).map_err(|source| {
        let fallback = bounded_log_root();
        BoundedCommandError::Io {
            source,
            path: fallback.clone(),
            log_directory: fallback,
        }
    })?;
    let stdout_log = log_directory.join("stdout.log");
    let stderr_log = log_directory.join("stderr.log");
    let stdout_file = fs::File::create(&stdout_log).map_err(|source| BoundedCommandError::Io {
        source,
        path: stdout_log.clone(),
        log_directory: log_directory.clone(),
    })?;
    let stderr_file = fs::File::create(&stderr_log).map_err(|source| BoundedCommandError::Io {
        source,
        path: stderr_log.clone(),
        log_directory: log_directory.clone(),
    })?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));
    let started = Instant::now();
    let mut child = command
        .spawn()
        .map_err(|source| BoundedCommandError::Spawn {
            source,
            log_directory: log_directory.clone(),
        })?;
    let child_id = child.id();
    let status = wait_until(&mut child, limit);
    let waited = started.elapsed();
    match status {
        Some(status) => {
            let output = read_logs(&stdout_log, &stderr_log, status, &log_directory)?;
            if output.status.success() {
                // Only this run's own directory: never a shared or fixture path.
                let _ = fs::remove_dir_all(&log_directory);
            }
            Ok(BoundedOutput {
                output,
                log_directory,
            })
        }
        None => {
            // Kill and reap only the child this harness spawned. Descendants are
            // out of scope: enumerating or killing them could hit unrelated work.
            let kill = child.kill();
            let exit = wait_until(&mut child, Duration::from_secs(2));
            let diagnostics = format!(
                "child pid {child_id} exceeded {limit:?}; killed and reaped only the owned process \
                 (kill={kill:?}, exit={exit:?}); descendant processes are not enumerated or killed; \
                 logs retained: stdout={} stderr={}; AMSI/Defender were not disabled or bypassed; \
                 no retry is attempted.",
                stdout_log.display(),
                stderr_log.display(),
            );
            Err(BoundedCommandError::TimedOut(TimedOut {
                limit,
                waited,
                child_id,
                log_directory,
                stdout_log,
                stderr_log,
                exit,
                diagnostics,
            }))
        }
    }
}

fn wait_until(child: &mut Child, limit: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + limit;
    let mut sleep = Duration::from_millis(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            // Not retried: a poll failure is reported as "did not exit".
            Err(_) => return None,
        }
        let now = Instant::now();
        if now >= deadline {
            // One last poll: the child may have exited between the two calls.
            return child.try_wait().ok().flatten();
        }
        thread::sleep(sleep.min(deadline - now));
        sleep = (sleep * 2).min(Duration::from_millis(50));
    }
}

fn read_logs(
    stdout_log: &std::path::Path,
    stderr_log: &std::path::Path,
    status: std::process::ExitStatus,
    log_directory: &std::path::Path,
) -> Result<std::process::Output, BoundedCommandError> {
    let read = |path: &std::path::Path| {
        fs::read(path).map_err(|source| BoundedCommandError::Io {
            source,
            path: path.to_path_buf(),
            log_directory: log_directory.to_path_buf(),
        })
    };
    Ok(std::process::Output {
        status,
        stdout: read(stdout_log)?,
        stderr: read(stderr_log)?,
    })
}
