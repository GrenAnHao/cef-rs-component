#![cfg(target_os = "windows")]
mod support;
use rust_cef_core::contracts::RuntimeFiles;
use rust_cef_runtime::deployment::validate_files;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use support::{Fixture, pe};

/// PowerShell here can hang while an inherited pipe keeps EOF open even after the
/// script exits, and rare AMSI access violations kill the compiler host. Both are
/// bounded by the shared helper; a timeout panics with the kept log path instead of
/// looking like an ordinary nonzero packager result.
fn run_powershell(command: Command, label: &str) -> Output {
    match support::run_bounded(command, label, support::DEFAULT_PS_TIMEOUT) {
        Ok(output) => output,
        Err(error) => panic!("packaging command `{label}` did not complete: {error}"),
    }
}

fn package(f: &Fixture, helper: &Path, output: &Path) -> Output {
    fs::write(
        f.root.join("CMakeLists.txt"),
        "# metadata fixture, never compiled",
    )
    .unwrap();
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/package-component.ps1");
    let mut command = Command::new("powershell.exe");
    command
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(script)
        .arg("-HostExecutable")
        .arg(&f.host)
        .arg("-HelperExecutable")
        .arg(helper)
        .arg("-CefPath")
        .arg(&f.root)
        .arg("-OutputDirectory")
        .arg(output)
        .args(["-HostBuildVersion", "independent-host-build"])
        .current_dir(&f.root);
    run_powershell(command, "package-component")
}

struct OutputRoot(PathBuf);
impl OutputRoot {
    fn new(f: &Fixture) -> Self {
        Self(f.root.with_file_name(format!(
            "{} 发行 output with spaces",
            f.root.file_name().unwrap().to_string_lossy()
        )))
    }
}
impl Drop for OutputRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn explicit_inputs_package_to_a_validated_distribution() {
    let f = Fixture::new();
    let owned_output = OutputRoot::new(&f);
    let output = owned_output.0.clone();
    let result = package(&f, &f.helper, &output);
    assert!(
        result.status.success(),
        "{} {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("component-runtime.json")).unwrap()).unwrap();
    assert_eq!(metadata["host"]["file_name"], "my host.exe");
    assert_eq!(metadata["host"]["build_version"], "independent-host-build");
    assert_eq!(metadata["helper"]["file_name"], "CEF child.exe");
    let files = RuntimeFiles {
        directory: output.clone(),
        helper_executable: output.join("CEF child.exe"),
    };
    validate_files(&files, &output.join("my host.exe")).unwrap();
}

#[test]
fn packaging_can_repeat_in_one_powershell_session() {
    let f = Fixture::new();
    fs::write(f.root.join("CMakeLists.txt"), "# fixture").unwrap();
    let owned_output = OutputRoot::new(&f);
    let driver = f.root.join("repeat.ps1");
    fs::write(&driver, r#"param($Tool, $HostExe, $ChildExe, $Sdk, $Out)
$ErrorActionPreference = 'Stop'
foreach ($name in @('one', 'two')) {
    & $Tool -HostExecutable $HostExe -HelperExecutable $ChildExe -CefPath $Sdk -OutputDirectory ([IO.Path]::Combine($Out, $name)) -HostBuildVersion 'repeat-host'
}
"#).unwrap();
    let tool =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/package-component.ps1");
    let mut command = Command::new("powershell.exe");
    command
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&driver)
        .arg(tool)
        .arg(&f.host)
        .arg(&f.helper)
        .arg(&f.root)
        .arg(&owned_output.0);
    let result = run_powershell(command, "package-component-repeat-session");
    assert!(
        result.status.success(),
        "{} {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    for name in ["one", "two"] {
        let directory = owned_output.0.join(name);
        validate_files(
            &RuntimeFiles {
                helper_executable: directory.join("CEF child.exe"),
                directory: directory.clone(),
            },
            &directory.join("my host.exe"),
        )
        .unwrap();
    }
}

#[test]
fn invalid_source_never_creates_a_successful_output_directory() {
    for case in ["architecture", "stamp", "resources", "same-name"] {
        let mut f = Fixture::new();
        let mut helper = f.helper.clone();
        match case {
            "architecture" => f.replace_helper(&pe(0x14c, true)),
            "stamp" => f.replace_helper(&pe(0x8664, false)),
            "resources" => fs::remove_file(f.root.join("resources.pak")).unwrap(),
            "same-name" => helper = f.host.clone(),
            _ => unreachable!(),
        }
        let owned_output = OutputRoot::new(&f);
        let output = owned_output.0.clone();
        let result = package(&f, &helper, &output);
        assert!(!result.status.success(), "case={case}");
        assert!(
            !output.exists(),
            "case={case}: validation must precede copying"
        );
    }
}

#[test]
fn output_inside_sdk_resources_is_rejected_before_writing() {
    let f = Fixture::new();
    let output = f.root.join("locales");
    let result = package(&f, &f.helper, &output);
    assert!(
        !result.status.success(),
        "packager wrote into its source locales"
    );
    assert!(!output.join("my host.exe").exists());
}

#[cfg(windows)]
#[test]
fn output_alias_to_sdk_resources_is_rejected_before_writing() {
    let f = Fixture::new();
    let alias = OutputRoot::new(&f);
    std::os::windows::fs::symlink_dir(f.root.join("locales"), &alias.0).unwrap();
    let result = package(&f, &f.helper, &alias.0);
    assert!(!result.status.success());
    assert!(!f.root.join("locales/my host.exe").exists());
}

#[test]
fn packaging_never_overwrites_the_source_sdk() {
    let f = Fixture::new();
    let original = fs::read(&f.host).unwrap();
    let result = package(&f, &f.helper, &f.root);
    assert!(!result.status.success());
    assert_eq!(fs::read(&f.host).unwrap(), original);
}
