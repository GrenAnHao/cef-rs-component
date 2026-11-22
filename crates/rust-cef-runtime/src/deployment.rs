//! Initialization-free validation of an explicitly selected distribution.
use std::{
    collections::HashSet,
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
};

use crate::build_stamp::{BINDING_REVISION, CEF_VERSION, COMPONENT_VERSION, HELPER_PREFIX, TARGET};
use rust_cef_core::contracts::RuntimeFiles;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

include!(concat!(env!("OUT_DIR"), "/helper_stamp.rs"));

pub const MANIFEST_NAME: &str = "component-runtime.json";
const MANIFEST_LIMIT: usize = 1024 * 1024;
pub const REQUIRED_RUNTIME_FILES: &[&str] = &[
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub file_name: String,
    pub build_version: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeFile {
    pub path: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentManifest {
    pub schema: u32,
    pub target: String,
    pub cef_version: String,
    pub binding_revision: String,
    pub component_version: String,
    pub host: Artifact,
    pub helper: Artifact,
    pub runtime: Vec<RuntimeFile>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeploymentError {
    EmptyHelperPath,
    PathNotAbsolute { path: PathBuf },
    MissingFile { path: PathBuf },
    InvalidLayout { path: PathBuf },
    InvalidPe { path: PathBuf },
    WrongArchitecture { path: PathBuf, machine: u16 },
    ManifestInvalid { path: PathBuf, reason: String },
    MetadataMismatch { field: &'static str },
    HashMismatch { path: PathBuf },
    MissingRequiredFile { path: PathBuf },
    InvalidManifestPath { path: PathBuf },
    HelperStampMismatch,
    Io { path: PathBuf, kind: io::ErrorKind },
}

impl std::fmt::Display for DeploymentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid CEF distribution: {self:?}")
    }
}
impl std::error::Error for DeploymentError {}

/// Only the validator constructs this value. Native startup must consume it first.
#[derive(Clone, Debug)]
pub struct ValidatedRuntimeFiles {
    directory: PathBuf,
    host_executable: PathBuf,
    helper_executable: PathBuf,
    manifest: DeploymentManifest,
}

impl ValidatedRuntimeFiles {
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub fn host_executable(&self) -> &Path {
        &self.host_executable
    }
    pub fn helper_executable(&self) -> &Path {
        &self.helper_executable
    }
    pub fn manifest(&self) -> &DeploymentManifest {
        &self.manifest
    }
}

fn io_error(path: &Path, error: io::Error) -> DeploymentError {
    if error.kind() == io::ErrorKind::NotFound {
        DeploymentError::MissingFile {
            path: path.to_owned(),
        }
    } else {
        DeploymentError::Io {
            path: path.to_owned(),
            kind: error.kind(),
        }
    }
}

fn absolute(path: &Path) -> Result<(), DeploymentError> {
    if !path.is_absolute() {
        return Err(DeploymentError::PathNotAbsolute {
            path: path.to_owned(),
        });
    }
    Ok(())
}

fn canonical_file(path: &Path) -> Result<PathBuf, DeploymentError> {
    if !fs::metadata(path).map_err(|e| io_error(path, e))?.is_file() {
        return Err(DeploymentError::MissingFile {
            path: path.to_owned(),
        });
    }
    path.canonicalize().map_err(|e| io_error(path, e))
}

fn relative_path(value: &str) -> Result<&Path, DeploymentError> {
    let path = Path::new(value);
    if value.is_empty() || !path.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(DeploymentError::InvalidManifestPath {
            path: path.to_owned(),
        });
    }
    Ok(path)
}

fn check_pe(path: &Path) -> Result<(), DeploymentError> {
    let invalid = || DeploymentError::InvalidPe {
        path: path.to_owned(),
    };
    let mut file = File::open(path).map_err(|e| io_error(path, e))?;
    let mut header = [0u8; 64];
    file.read_exact(&mut header).map_err(|_| invalid())?;
    if &header[..2] != b"MZ" {
        return Err(invalid());
    }
    let offset = u32::from_le_bytes(header[60..64].try_into().unwrap()) as u64;
    file.seek(SeekFrom::Start(offset)).map_err(|_| invalid())?;
    let mut signature = [0u8; 6];
    file.read_exact(&mut signature).map_err(|_| invalid())?;
    if &signature[..4] != b"PE\0\0" {
        return Err(invalid());
    }
    let machine = u16::from_le_bytes([signature[4], signature[5]]);
    if machine != 0x8664 {
        return Err(DeploymentError::WrongArchitecture {
            path: path.to_owned(),
            machine,
        });
    }
    Ok(())
}

pub fn file_sha256(path: &Path) -> Result<String, DeploymentError> {
    let mut file = File::open(path).map_err(|e| io_error(path, e))?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer).map_err(|e| io_error(path, e))?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn check_hash(path: &Path, expected: &str) -> Result<(), DeploymentError> {
    if !file_sha256(path)?.eq_ignore_ascii_case(expected) {
        return Err(DeploymentError::HashMismatch {
            path: path.to_owned(),
        });
    }
    Ok(())
}

fn check_helper_stamp(path: &Path) -> Result<(), DeploymentError> {
    let mut file = File::open(path).map_err(|e| io_error(path, e))?;
    let mut chunk = [0u8; 65536];
    let mut buffer = Vec::with_capacity(chunk.len() + HELPER_STAMP_LEN);
    let mut tail = Vec::with_capacity(HELPER_STAMP_LEN - 1);
    loop {
        let n = file.read(&mut chunk).map_err(|e| io_error(path, e))?;
        if n == 0 {
            return Err(DeploymentError::HelperStampMismatch);
        }
        buffer.clear();
        buffer.extend_from_slice(&tail);
        buffer.extend_from_slice(&chunk[..n]);
        for candidate in buffer.windows(HELPER_STAMP_LEN) {
            if candidate.starts_with(HELPER_PREFIX)
                && Sha256::digest(candidate)[..] == HELPER_STAMP_DIGEST
            {
                return Ok(());
            }
        }
        tail.clear();
        let start = buffer.len().saturating_sub(HELPER_STAMP_LEN - 1);
        tail.extend_from_slice(&buffer[start..]);
    }
}

fn read_manifest(path: &Path) -> Result<DeploymentManifest, DeploymentError> {
    let file = File::open(path).map_err(|e| io_error(path, e))?;
    let mut bytes = Vec::new();
    file.take((MANIFEST_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| io_error(path, e))?;
    if bytes.len() > MANIFEST_LIMIT {
        return Err(DeploymentError::ManifestInvalid {
            path: path.to_owned(),
            reason: "manifest exceeds 1 MiB".into(),
        });
    }
    serde_json::from_slice(&bytes).map_err(|e| DeploymentError::ManifestInvalid {
        path: path.to_owned(),
        reason: e.to_string(),
    })
}

fn artifact_metadata(
    artifact: &Artifact,
    actual: &Path,
    field: &'static str,
) -> Result<(), DeploymentError> {
    let name = relative_path(&artifact.file_name)?;
    if name.components().count() != 1
        || name.file_name() != actual.file_name()
        || artifact.build_version.trim().is_empty()
    {
        return Err(DeploymentError::MetadataMismatch { field });
    }
    Ok(())
}

/// No initialization, DLL searching, environment fallback, or process launch occurs here.
pub fn validate_files(
    files: &RuntimeFiles,
    host_executable: &Path,
) -> Result<ValidatedRuntimeFiles, DeploymentError> {
    if files.helper_executable.as_os_str().is_empty() {
        return Err(DeploymentError::EmptyHelperPath);
    }
    absolute(&files.directory)?;
    absolute(&files.helper_executable)?;
    absolute(host_executable)?;
    let directory = files
        .directory
        .canonicalize()
        .map_err(|e| io_error(&files.directory, e))?;
    if !directory.is_dir() {
        return Err(DeploymentError::InvalidLayout { path: directory });
    }
    let helper_executable = canonical_file(&files.helper_executable)?;
    let host_executable = canonical_file(host_executable)?;
    for exe in [&host_executable, &helper_executable] {
        if exe.parent() != Some(directory.as_path()) {
            return Err(DeploymentError::InvalidLayout {
                path: exe.to_owned(),
            });
        }
        check_pe(exe)?;
    }
    if helper_executable == host_executable {
        return Err(DeploymentError::InvalidLayout {
            path: helper_executable,
        });
    }
    let manifest_path = canonical_file(&directory.join(MANIFEST_NAME))?;
    if manifest_path.parent() != Some(directory.as_path()) {
        return Err(DeploymentError::InvalidLayout {
            path: manifest_path,
        });
    }
    let manifest = read_manifest(&manifest_path)?;
    for (field, good) in [
        ("schema", manifest.schema == 1),
        ("target", manifest.target == TARGET),
        ("cef_version", manifest.cef_version == CEF_VERSION),
        (
            "binding_revision",
            manifest.binding_revision == BINDING_REVISION,
        ),
        (
            "component_version",
            manifest.component_version == COMPONENT_VERSION,
        ),
        (
            "helper.build_version",
            manifest.helper.build_version == COMPONENT_VERSION,
        ),
    ] {
        if !good {
            return Err(DeploymentError::MetadataMismatch { field });
        }
    }
    artifact_metadata(&manifest.host, &host_executable, "host")?;
    artifact_metadata(&manifest.helper, &helper_executable, "helper")?;
    let mut seen = HashSet::new();
    for entry in &manifest.runtime {
        relative_path(&entry.path)?;
        // Windows distribution identity is case-insensitive even on metadata-only hosts.
        if !seen.insert(entry.path.replace('\\', "/").to_ascii_lowercase()) {
            return Err(DeploymentError::ManifestInvalid {
                path: manifest_path.clone(),
                reason: "duplicate runtime path".into(),
            });
        }
    }
    for required in REQUIRED_RUNTIME_FILES {
        if !seen.contains(&required.to_ascii_lowercase()) {
            return Err(DeploymentError::MissingRequiredFile {
                path: PathBuf::from(required),
            });
        }
    }
    check_hash(&host_executable, &manifest.host.sha256)?;
    check_hash(&helper_executable, &manifest.helper.sha256)?;
    for entry in &manifest.runtime {
        let path = canonical_file(&directory.join(&entry.path))?;
        if !path.starts_with(&directory) {
            return Err(DeploymentError::InvalidLayout { path });
        }
        // A symlink's physical suffix cannot change the logical DLL role.
        if Path::new(&entry.path)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("dll"))
        {
            check_pe(&path)?;
        }
        check_hash(&path, &entry.sha256)?;
    }
    let archive: serde_json::Value = serde_json::from_slice(
        &fs::read(directory.join("archive.json")).map_err(|e| io_error(&directory, e))?,
    )
    .map_err(|e| DeploymentError::ManifestInvalid {
        path: directory.join("archive.json"),
        reason: e.to_string(),
    })?;
    if !archive
        .get("name")
        .and_then(|v| v.as_str())
        .is_some_and(|name| {
            name.starts_with(&format!("cef_binary_{CEF_VERSION}+")) && name.contains("_windows64_")
        })
    {
        return Err(DeploymentError::MetadataMismatch {
            field: "archive.name",
        });
    }
    check_helper_stamp(&helper_executable)?;
    Ok(ValidatedRuntimeFiles {
        directory,
        host_executable,
        helper_executable,
        manifest,
    })
}
