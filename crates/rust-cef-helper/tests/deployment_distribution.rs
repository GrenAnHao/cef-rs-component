//! A real, self-hosted deployment test. No synthetic PE or pixel claims.
//! The copied host validates the actual distribution from another cwd; the
//! real helper must reject a bare invocation. It never initializes a Browser.
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod windows {
    use rust_cef_runtime::{contracts::RuntimeFiles, deployment::validate_files};
    use std::{
        env, fs,
        path::{Path, PathBuf},
        process::{Command, Output},
        time::{SystemTime, UNIX_EPOCH},
    };

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetErrorMode(mode: u32) -> u32;
    }
    struct ErrorMode(u32);
    impl Drop for ErrorMode {
        fn drop(&mut self) {
            unsafe {
                SetErrorMode(self.0);
            }
        }
    }

    fn clean_command(exe: &Path, cwd: &Path) -> Command {
        let mut command = Command::new(exe);
        command.current_dir(cwd);
        for (key, _) in env::vars_os() {
            let text = key.to_string_lossy().to_ascii_uppercase();
            if text.starts_with("POC_")
                || text.starts_with("CARGO_")
                || text == "CEF_PATH"
                || text == "START_URL"
            {
                command.env_remove(key);
            }
        }
        let windows = PathBuf::from(env::var_os("SystemRoot").expect("Windows system directory"));
        command.env(
            "PATH",
            env::join_paths([windows.join("System32"), windows]).unwrap(),
        );
        command
    }

    fn describe(output: &Output) -> String {
        format!(
            "exit={:?}\nstdout={}\nstderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    pub fn run() {
        let arguments = env::args_os().skip(1).collect::<Vec<_>>();
        if arguments
            .first()
            .is_some_and(|s| s == "--validate-distribution")
        {
            assert_eq!(arguments.len(), 2);
            let host = env::current_exe().unwrap().canonicalize().unwrap();
            let directory = host.parent().unwrap().to_path_buf();
            let helper = directory.join(&arguments[1]);
            let files = RuntimeFiles {
                directory,
                helper_executable: helper,
            };
            let validated = validate_files(&files, &host).unwrap();
            println!(
                "DISTRIBUTION_VALIDATED pid={} helper={}",
                std::process::id(),
                validated.helper_executable().display()
            );
            return;
        }
        assert!(arguments.is_empty() || arguments == ["--nocapture"]);
        // Suppress loader dialogs only in this test process and its children.
        // No system setting or unrelated process is changed.
        let _mode = ErrorMode(unsafe { SetErrorMode(0x0001 | 0x0002 | 0x8000) });
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = source
            .join("target/component-validation/deployment")
            .join(format!("{}-{stamp}", std::process::id()));
        let bundle = root.join("发行 bundle with spaces");
        let other_cwd = root.join("unrelated cwd");
        fs::create_dir_all(&other_cwd).unwrap();
        let host = env::current_exe().unwrap();
        let helper = PathBuf::from(env!("CARGO_BIN_EXE_rust-cef-helper"));
        let cef = env::var_os("CEF_PATH").expect("CEF_PATH must explicitly select the test SDK");
        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(source.join("scripts/package-component.ps1"))
            .arg("-HostExecutable")
            .arg(&host)
            .arg("-HelperExecutable")
            .arg(&helper)
            .arg("-CefPath")
            .arg(&cef)
            .arg("-OutputDirectory")
            .arg(&bundle)
            .args(["-HostBuildVersion", "native-deployment-integration"])
            .current_dir(&other_cwd)
            .output()
            .unwrap();
        fs::write(root.join("package.log"), describe(&output)).unwrap();
        assert!(output.status.success(), "{}", describe(&output));
        let copied_host = bundle.join(host.file_name().unwrap());
        let copied_helper = bundle.join(helper.file_name().unwrap());
        let validated = clean_command(&copied_host, &other_cwd)
            .arg("--validate-distribution")
            .arg(helper.file_name().unwrap())
            .output()
            .unwrap();
        fs::write(root.join("validate.log"), describe(&validated)).unwrap();
        assert!(validated.status.success(), "{}", describe(&validated));
        assert!(String::from_utf8_lossy(&validated.stdout).contains("DISTRIBUTION_VALIDATED"));
        // Linking the validator must not make an ordinary host advertise itself
        // as a helper. Rename its image to bypass only the file-name check.
        let impostor = root.join("renamed ordinary host.exe");
        fs::copy(&host, &impostor).unwrap();
        let bad_output = root.join("must not package an ordinary host");
        let bad_helper = Command::new("powershell.exe")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(source.join("scripts/package-component.ps1"))
            .arg("-HostExecutable")
            .arg(&host)
            .arg("-HelperExecutable")
            .arg(&impostor)
            .arg("-CefPath")
            .arg(&cef)
            .arg("-OutputDirectory")
            .arg(&bad_output)
            .args(["-HostBuildVersion", "negative-host-helper"])
            .current_dir(&other_cwd)
            .output()
            .unwrap();
        fs::write(
            root.join("ordinary-host-as-helper.log"),
            describe(&bad_helper),
        )
        .unwrap();
        assert!(!bad_helper.status.success(), "{}", describe(&bad_helper));
        assert!(!bad_output.exists());
        let rejected = clean_command(&copied_helper, &other_cwd).output().unwrap();
        fs::write(root.join("bare-helper.log"), describe(&rejected)).unwrap();
        assert_eq!(rejected.status.code(), Some(2), "{}", describe(&rejected));
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("nonempty CEF --type"));
        let dll = bundle.join("libcef.dll");
        let backup = bundle.join("libcef.dll.saved-for-test");
        fs::rename(&dll, &backup).unwrap();
        let missing = clean_command(&copied_helper, &other_cwd).output();
        fs::rename(&backup, &dll).unwrap();
        let diagnostic = match missing {
            Ok(output) => {
                assert!(!output.status.success());
                assert_ne!(
                    output.status.code(),
                    Some(2),
                    "Expected loader rejection, not our Rust type guard"
                );
                describe(&output)
            }
            Err(error) => format!("external loader launch failure: {error:?}"),
        };
        fs::write(root.join("missing-dll.log"), diagnostic).unwrap();
        fs::copy(
            bundle.join("component-runtime.json"),
            root.join("validated-manifest.json"),
        )
        .unwrap();
        println!(
            "DEPLOYMENT_INTEGRATION_RESULT=ok evidence={}",
            root.display()
        );
        // Leave compact logs/manifest; all large copied bytes were test-owned.
        fs::remove_dir_all(bundle).unwrap();
    }
}

fn main() {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    windows::run();
    #[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
    panic!("Native deployment integration requires Windows x64; core-only checks are independent");
}
