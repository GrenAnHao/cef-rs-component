mod support;

use rust_cef_runtime::deployment::{DeploymentError, validate_files};
use serde_json::json;
use std::{fs, path::PathBuf};
use support::{Fixture, pe};

#[test]
fn complete_metadata_fixture_is_accepted_without_initialization() {
    let f = Fixture::new();
    let validated = validate_files(&f.files(), &f.host).unwrap();
    assert_eq!(validated.helper_executable(), f.helper);
    assert_eq!(validated.directory(), f.root);
}

#[test]
fn helper_path_must_be_explicit_absolute_and_present() {
    let f = Fixture::new();
    let mut files = f.files();
    files.helper_executable = PathBuf::new();
    assert!(matches!(
        validate_files(&files, &f.host),
        Err(DeploymentError::EmptyHelperPath)
    ));
    files.helper_executable = PathBuf::from("CEF child.exe");
    assert!(matches!(
        validate_files(&files, &f.host),
        Err(DeploymentError::PathNotAbsolute { .. })
    ));
    files.helper_executable = f.root.join("absent.exe");
    assert!(matches!(
        validate_files(&files, &f.host),
        Err(DeploymentError::MissingFile { .. })
    ));
}

#[test]
fn runtime_root_and_host_must_be_absolute_and_colocated() {
    let f = Fixture::new();
    let mut files = f.files();
    files.directory = PathBuf::from("relative-root");
    assert!(matches!(
        validate_files(&files, &f.host),
        Err(DeploymentError::PathNotAbsolute { .. })
    ));
    let other = Fixture::new();
    assert!(matches!(
        validate_files(&f.files(), &other.host),
        Err(DeploymentError::InvalidLayout { .. })
    ));
    fs::create_dir(f.root.join("nested")).unwrap();
    files = f.files();
    files.helper_executable = f.root.join("nested/child.exe");
    fs::copy(&f.helper, &files.helper_executable).unwrap();
    assert!(matches!(
        validate_files(&files, &f.host),
        Err(DeploymentError::InvalidLayout { .. })
    ));
}

#[test]
fn helper_architecture_and_pe_header_are_checked() {
    let mut f = Fixture::new();
    f.replace_helper(&pe(0x14c, true));
    assert!(matches!(
        validate_files(&f.files(), &f.host),
        Err(DeploymentError::WrongArchitecture { .. })
    ));
    f.replace_helper(b"not a PE file");
    assert!(matches!(
        validate_files(&f.files(), &f.host),
        Err(DeploymentError::InvalidPe { .. })
    ));
}

#[cfg(windows)]
#[test]
fn logical_dll_alias_still_requires_a_valid_x64_pe() {
    for (bytes, wrong_arch) in [(b"not a PE".to_vec(), false), (pe(0x14c, false), true)] {
        let mut f = Fixture::new();
        let blob = f.root.join("blob.bin");
        fs::write(&blob, &bytes).unwrap();
        let dll = f.root.join("libcef.dll");
        fs::remove_file(&dll).unwrap();
        std::os::windows::fs::symlink_file(&blob, &dll).unwrap();
        f.manifest["runtime"][0]["sha256"] = json!(support::digest(&bytes));
        f.save();
        let result = validate_files(&f.files(), &f.host);
        if wrong_arch {
            assert!(matches!(
                result,
                Err(DeploymentError::WrongArchitecture { .. })
            ));
        } else {
            assert!(matches!(result, Err(DeploymentError::InvalidPe { .. })));
        }
    }
}

#[test]
fn metadata_versions_and_schema_must_match() {
    for (field, bad) in [
        ("schema", json!(2)),
        ("target", json!("aarch64-pc-windows-msvc")),
        ("cef_version", json!("156.0.32")),
        ("binding_revision", json!("different")),
        ("component_version", json!("other")),
    ] {
        let mut f = Fixture::new();
        f.manifest[field] = bad;
        f.save();
        assert!(
            matches!(
                validate_files(&f.files(), &f.host),
                Err(DeploymentError::MetadataMismatch { .. })
            ),
            "field={field}"
        );
    }
}

#[test]
fn hashes_cover_actual_host_helper_and_runtime_bytes() {
    for name in [
        "my host.exe",
        "CEF child.exe",
        "resources.pak",
        "locales/en-US.pak",
    ] {
        let f = Fixture::new();
        let path = f.root.join(name);
        let mut bytes = fs::read(&path).unwrap();
        bytes.extend_from_slice(b"changed");
        fs::write(path, bytes).unwrap();
        assert!(
            matches!(
                validate_files(&f.files(), &f.host),
                Err(DeploymentError::HashMismatch { .. })
            ),
            "file={name}"
        );
    }
}

#[test]
fn matching_hash_is_not_enough_for_a_non_component_helper() {
    let mut f = Fixture::new();
    f.replace_helper(&pe(0x8664, false));
    assert!(matches!(
        validate_files(&f.files(), &f.host),
        Err(DeploymentError::HelperStampMismatch)
    ));
}

#[test]
fn helper_stamp_may_cross_a_read_buffer_boundary() {
    let mut f = Fixture::new();
    let mut bytes = pe(0x8664, false);
    bytes.resize(65530, 0);
    bytes.extend_from_slice(support::STAMP.as_bytes());
    f.replace_helper(&bytes);
    validate_files(&f.files(), &f.host).unwrap();
}

#[test]
fn incomplete_stamp_is_not_a_component_helper() {
    let mut f = Fixture::new();
    let mut bytes = pe(0x8664, false);
    bytes.extend_from_slice(b"GPUI-CEF-COMPONENT|helper|schema=1|");
    f.replace_helper(&bytes);
    assert!(matches!(
        validate_files(&f.files(), &f.host),
        Err(DeploymentError::HelperStampMismatch)
    ));
}

#[test]
fn oversized_manifest_is_rejected_before_parsing() {
    let f = Fixture::new();
    fs::write(
        f.root.join("component-runtime.json"),
        vec![b' '; 1024 * 1024 + 1],
    )
    .unwrap();
    assert!(matches!(
        validate_files(&f.files(), &f.host),
        Err(DeploymentError::ManifestInvalid { .. })
    ));
}

#[test]
fn required_resources_cannot_be_omitted_from_manifest() {
    let mut f = Fixture::new();
    f.manifest["runtime"]
        .as_array_mut()
        .unwrap()
        .retain(|r| r["path"] != "resources.pak");
    f.save();
    assert!(matches!(
        validate_files(&f.files(), &f.host),
        Err(DeploymentError::MissingRequiredFile { .. })
    ));
}

#[test]
fn missing_or_malformed_manifest_does_not_fall_back() {
    let f = Fixture::new();
    let path = f.root.join("component-runtime.json");
    fs::write(&path, b"{").unwrap();
    assert!(matches!(
        validate_files(&f.files(), &f.host),
        Err(DeploymentError::ManifestInvalid { .. })
    ));
    fs::remove_file(path).unwrap();
    assert!(matches!(
        validate_files(&f.files(), &f.host),
        Err(DeploymentError::MissingFile { .. })
    ));
}

#[test]
fn manifest_paths_cannot_escape_runtime_root() {
    let mut f = Fixture::new();
    f.manifest["runtime"][0]["path"] = json!("../outside.dll");
    f.save();
    assert!(matches!(
        validate_files(&f.files(), &f.host),
        Err(DeploymentError::InvalidManifestPath { .. })
    ));
}
