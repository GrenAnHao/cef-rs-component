#![cfg(all(target_os = "windows", target_arch = "x86_64"))]
use rust_cef_runtime::profile_paths::{
    ProfilePaths, RootLease, StoragePathError, paths_equal, prepare_directory,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cef-paths-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "path/lease fixture retained after failure: {}",
                self.0.display()
            );
        } else {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn wait_for(path: &Path, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !path.exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "child exited before signaling readiness"
        );
        assert!(Instant::now() < deadline, "child readiness timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn path_validation_and_registration() {
    let fixture = Fixture::new();
    let root = prepare_directory(&fixture.0.join("root")).unwrap();
    assert_eq!(
        prepare_directory(Path::new("relative")),
        Err(StoragePathError::NotAbsolute)
    );
    assert_eq!(
        prepare_directory(Path::new(r"\\.\C:\bad")),
        Err(StoragePathError::NotAbsolute)
    );
    use std::os::windows::ffi::OsStringExt;
    let invalid = root.join(std::ffi::OsString::from_wide(&[0xd800]));
    assert_eq!(
        prepare_directory(&invalid),
        Err(StoragePathError::InvalidUnicode)
    );
    // PathBuf::push normalizes parents on verbatim roots before our API sees
    // them, so construct actual unnormalized input rather than a sanitized path.
    for suffix in [r"\x\..\y", "/x/../y"] {
        let raw = PathBuf::from(format!("{}{suffix}", root.to_str().unwrap()));
        assert!(raw.to_str().unwrap().contains(".."));
        assert_eq!(prepare_directory(&raw), Err(StoragePathError::Traversal));
    }
    let mut paths = ProfilePaths::new(root.clone());
    assert_eq!(
        paths.register("root", &root),
        Err(StoragePathError::ReservedDefault)
    );
    assert_eq!(
        paths.register("default", &root.join("default/a")),
        Err(StoragePathError::ReservedDefault)
    );
    assert_eq!(
        paths.register("outside", &fixture.0.join("outside")),
        Err(StoragePathError::NotDirectChild)
    );
    assert!(!fixture.0.join("outside").exists());
    assert_eq!(
        paths.register("nested", &root.join("profiles/P1")),
        Err(StoragePathError::NotDirectChild)
    );
    assert!(!root.join("profiles").exists());
    for name in ["Default", "System Profile", "Guest Profile"] {
        assert_eq!(
            paths.register("reserved", &root.join(name)),
            Err(StoragePathError::ReservedDefault)
        );
    }
    let first = paths.register("one", &root.join("Alpha")).unwrap();
    assert!(paths_equal(
        &first,
        &paths.register("one", &root.join("alpha")).unwrap()
    ));
    assert_eq!(
        paths.register("two", &root.join("ALPHA")),
        Err(StoragePathError::Overlap)
    );
    assert_eq!(
        paths.register("two", &root.join("Alpha/child")),
        Err(StoragePathError::NotDirectChild)
    );
    assert_eq!(
        paths.register("one", &root.join("other")),
        Err(StoragePathError::Overlap)
    );
    assert_eq!(
        paths.register("two", &root.join("other/child")),
        Err(StoragePathError::NotDirectChild)
    );
    paths.register("two", &root.join("other")).unwrap();
    assert_eq!(
        paths.register("three", &root.join("other")),
        Err(StoragePathError::Overlap)
    );
}

#[test]
fn junctions_are_rejected() {
    let fixture = Fixture::new();
    let root = prepare_directory(&fixture.0.join("root")).unwrap();
    let outside = fixture.0.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let link = root.join("junction");
    let output = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(&outside)
        .output()
        .expect("run mklink /J");
    assert!(
        output.status.success(),
        "junction creation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        prepare_directory(&link.join("child")),
        Err(StoragePathError::Reparse)
    );
    fs::remove_dir(&link).unwrap();
    let lease_link = root.join(".rust-cef-gpui.lease");
    let output = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&lease_link)
        .arg(&outside)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "lease junction creation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(matches!(
        RootLease::acquire(&root),
        Err(StoragePathError::Reparse)
    ));
    fs::remove_dir(&lease_link).unwrap();
}

#[test]
fn lease_child() {
    let Some(root) = std::env::var_os("GPUI_CEF_LEASE_TEST_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let mut lease = RootLease::acquire(&root).unwrap();
    if std::env::var_os("GPUI_CEF_LEASE_ABANDON").is_some() {
        lease.abandon();
    }
    fs::write(root.join("ready"), b"ready").unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !root.join("stop").exists() {
        assert!(Instant::now() < deadline, "parent did not release child");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn abandoned_lease_stays_locked_until_owning_process_exits() {
    let fixture = Fixture::new();
    let root = prepare_directory(&fixture.0.join("root")).unwrap();
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "lease_child", "--nocapture"])
        .env("GPUI_CEF_LEASE_TEST_ROOT", &root)
        .env("GPUI_CEF_LEASE_ABANDON", "1")
        .stdout(fs::File::create(root.join("lease-child.stdout.log")).unwrap())
        .stderr(fs::File::create(root.join("lease-child.stderr.log")).unwrap())
        .spawn()
        .unwrap();
    let mut child = OwnedChild(child);
    wait_for(&root.join("ready"), &mut child.0);
    assert!(matches!(
        RootLease::acquire(&root),
        Err(StoragePathError::LeaseInUse)
    ));
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    assert!(RootLease::acquire(&root).is_ok());
}

#[test]
fn short_path_api_alias_cannot_register_a_second_profile() {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetShortPathNameW(input: *const u16, output: *mut u16, len: u32) -> u32;
    }
    let fixture = Fixture::new();
    let root = prepare_directory(&fixture.0.join("root")).unwrap();
    let mut paths = ProfilePaths::new(root.clone());
    let original = paths
        .register("P1", &root.join("Long Profile Alias Directory"))
        .unwrap();
    let input: Vec<u16> = original.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut output = vec![0u16; 32768];
    // SAFETY: NUL-terminated input and a writable buffer of the supplied length.
    let count =
        unsafe { GetShortPathNameW(input.as_ptr(), output.as_mut_ptr(), output.len() as u32) }
            as usize;
    assert!(count > 0 && count < output.len());
    let alias = PathBuf::from(std::ffi::OsString::from_wide(&output[..count]));
    // On a volume without short-name creation the API may return the long name;
    // it still must not admit the same physical directory twice.
    assert_eq!(paths.register("P2", &alias), Err(StoragePathError::Overlap));
}

#[test]
fn lease_excludes_other_process_and_preserves_data() {
    let fixture = Fixture::new();
    let root = prepare_directory(&fixture.0.join("root")).unwrap();
    fs::write(root.join("host-data"), b"keep").unwrap();
    fs::write(root.join(".rust-cef-gpui.lease"), b"keep lease data").unwrap();
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "lease_child", "--nocapture"])
        .env("GPUI_CEF_LEASE_TEST_ROOT", &root)
        .stdout(fs::File::create(root.join("lease-child.stdout.log")).unwrap())
        .stderr(fs::File::create(root.join("lease-child.stderr.log")).unwrap())
        .spawn()
        .unwrap();
    let mut child = OwnedChild(child);
    wait_for(&root.join("ready"), &mut child.0);
    assert!(matches!(
        RootLease::acquire(&root),
        Err(StoragePathError::LeaseInUse)
    ));
    fs::write(root.join("stop"), b"stop").unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "child exit timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut lease = RootLease::acquire(&root).unwrap();
    assert!(paths_equal(lease.root(), &root));
    lease.release();
    drop(RootLease::acquire(&root).unwrap());
    assert_eq!(fs::read(root.join("host-data")).unwrap(), b"keep");
    assert_eq!(
        fs::read(root.join(".rust-cef-gpui.lease")).unwrap(),
        b"keep lease data"
    );
}
