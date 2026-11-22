#![cfg(feature = "test-support")]
use rust_cef_runtime::test_support::{ProbeOptions, ProbeReport};

fn arguments(mode: &str, backend: &str) -> Vec<String> {
    vec![
        "--mode".into(),
        mode.into(),
        "--backend".into(),
        backend.into(),
        "--report".into(),
        std::env::temp_dir()
            .join("probe report.json")
            .to_string_lossy()
            .into_owned(),
        "--helper".into(),
        std::env::temp_dir()
            .join("CEF helper.exe")
            .to_string_lossy()
            .into_owned(),
    ]
}

#[test]
fn probe_requires_one_explicit_mode_backend_and_absolute_paths() {
    let options = ProbeOptions::parse(arguments("Threaded", "Software")).unwrap();
    assert_eq!(options.mode_name(), "Threaded");
    assert_eq!(options.backend_name(), "Software");
    assert!(ProbeOptions::parse(arguments("All", "Software")).is_err());
    assert!(ProbeOptions::parse(arguments("Integrated", "Auto")).is_err());
    assert!(ProbeOptions::parse(Vec::<String>::new()).is_err());
    let mut relative = arguments("Integrated", "Software");
    *relative.last_mut().unwrap() = "helper.exe".into();
    assert!(ProbeOptions::parse(relative).is_err());
    let mut unknown = arguments("Integrated", "Software");
    unknown.extend(["--bogus".into(), "value".into()]);
    assert!(ProbeOptions::parse(unknown).is_err());
}

#[test]
fn native_shutdown_scenarios_are_explicit_and_bounded() {
    for name in [
        "Smoke",
        "Multiple",
        "Creating",
        "Loading",
        "PendingRequest",
        "InFlight",
        "CloseTimeout",
    ] {
        let mut args = arguments("Threaded", "Software");
        args.extend(["--scenario".into(), name.into()]);
        assert!(ProbeOptions::parse(args).is_ok(), "scenario={name}");
    }
    let mut args = arguments("Integrated", "Software");
    args.extend(["--scenario".into(), "All".into()]);
    assert!(
        ProbeOptions::parse(args).is_err(),
        "All must expand in the launcher, not hot-switch one process"
    );
}

#[test]
fn observer_handshake_accepts_only_an_explicit_absolute_gate() {
    let mut absolute = arguments("Integrated", "Software");
    absolute.extend([
        "--observer-gate".into(),
        std::env::temp_dir()
            .join("observer.release")
            .to_string_lossy()
            .into_owned(),
    ]);
    assert!(ProbeOptions::parse(absolute).is_ok());
    let mut relative = arguments("Integrated", "Software");
    relative.extend(["--observer-gate".into(), "observer.release".into()]);
    assert!(ProbeOptions::parse(relative).is_err());
}

#[test]
fn observer_ready_is_published_only_as_a_complete_file() {
    let root = std::env::temp_dir().join(format!("observer-publication-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    for index in 0..20 {
        let gate = root.join(format!("{index}.release"));
        let ready = gate.with_extension("ready");
        let reader = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !ready.exists() {
                assert!(std::time::Instant::now() < deadline);
                std::thread::yield_now();
            }
            assert_eq!(std::fs::read_to_string(ready).unwrap(), "12345");
        });
        rust_cef_runtime::test_support::publish_observer_ready(&gate, 12345).unwrap();
        reader.join().unwrap();
        assert!(!gate.with_extension("ready.tmp").exists());
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn requested_configuration_is_not_observed_native_success() {
    let options = ProbeOptions::parse(arguments("Threaded", "D3d11")).unwrap();
    let report = ProbeReport::started(&options, 123);
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["requested_mode"], "Threaded");
    assert_eq!(json["requested_backend"], "D3d11");
    assert!(json["actual_mode"].is_null());
    assert!(json["actual_backend"].is_null());
    assert!(json["cef_ui_thread_id"].is_null());
    assert!(json["app_reference_retained"].is_null());
    assert!(json["exit_code"].is_null());
    assert!(!report.native_success());
    assert!(report.pid > 0);
    assert_eq!(report.main_thread_id, 123);
}

#[test]
fn report_success_requires_observed_threads_frames_and_closed_instances() {
    // Synthetic report values exercise only wire validation, not a CEF run.
    let options = ProbeOptions::parse(arguments("Threaded", "Software")).unwrap();
    let mut report = ProbeReport::started(&options, 123);
    report.actual_mode = Some("Threaded");
    report.actual_backend = Some("Software");
    report.init_thread_id = Some(123);
    report.cef_ui_thread_id = Some(456);
    report.shutdown_thread_id = Some(123);
    report.app_reference_retained = Some(false);
    report.exit_code = Some(0);
    report.tasks_posted = 2;
    report.tasks_completed = 2;
    report.created = 1;
    report.closed = 1;
    report.valid_frame_browsers = 1;
    report.cef_shutdown_calls = 1;
    report.native_calls = 1;
    report.post_shutdown_rejections = 2;
    report.frames = 1;
    report.first_frame_width = 320;
    report.first_frame_height = 240;
    assert!(report.native_success());
    report.manual_pump_count = 1;
    assert!(!report.native_success());
    report.manual_pump_count = 0;
    report.cef_ui_thread_id = Some(123);
    assert!(!report.native_success());
    report.cef_ui_thread_id = Some(456);
    report.closed = 0;
    assert!(!report.native_success());
    report.closed = 1;
    report.first_frame_width = 0;
    assert!(!report.native_success());
    report.first_frame_width = 320;
    report.pending_tasks = 1;
    assert!(!report.native_success());
    report.pending_tasks = 0;
    report.app_reference_retained = Some(true);
    assert!(!report.native_success());
}
