//! Initialization-free validation of an explicitly selected distribution.
use std::{
    collections::HashSet,
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use crate::build_stamp::{BINDING_REVISION, CEF_VERSION, COMPONENT_VERSION, HELPER_PREFIX, TARGET};
use rust_cef_core::contracts::RuntimeFiles;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const HASH_BUFFER_BYTES: usize = 256 * 1024;

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

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod cng_sha256 {
    use std::{ffi::c_void, fs::File, io::Read, sync::OnceLock};

    const SHA256_ALG_NAME: &[u16] = &[
        b'S' as u16, b'H' as u16, b'A' as u16, b'2' as u16, b'5' as u16, b'6' as u16, 0,
    ];
    const OBJECT_LENGTH_PROP: &[u16] = &[
        b'O' as u16, b'b' as u16, b'j' as u16, b'e' as u16, b'c' as u16, b't' as u16,
        b'L' as u16, b'e' as u16, b'n' as u16, b'g' as u16, b't' as u16, b'h' as u16, 0,
    ];
    const SHA256_DIGEST_LEN: u32 = 32;

    #[link(name = "bcrypt")]
    unsafe extern "system" {
        fn BCryptOpenAlgorithmProvider(
            ph_algorithm: *mut *mut c_void,
            psz_alg_id: *const u16,
            psz_implementation: *const u16,
            dw_flags: u32,
        ) -> i32;
        fn BCryptGetProperty(
            h_object: *mut c_void,
            psz_property: *const u16,
            pb_output: *mut u8,
            cb_output: u32,
            pcb_result: *mut u32,
            dw_flags: u32,
        ) -> i32;
        fn BCryptCloseAlgorithmProvider(h_algorithm: *mut c_void, dw_flags: u32) -> i32;
        fn BCryptCreateHash(
            h_algorithm: *mut c_void,
            ph_hash: *mut *mut c_void,
            pb_hash_object: *mut u8,
            cb_hash_object: u32,
            pb_secret: *mut u8,
            cb_secret: u32,
            dw_flags: u32,
        ) -> i32;
        fn BCryptHashData(
            h_hash: *mut c_void,
            pb_input: *const u8,
            cb_input: u32,
            dw_flags: u32,
        ) -> i32;
        fn BCryptFinishHash(
            h_hash: *mut c_void,
            pb_output: *mut u8,
            cb_output: u32,
            dw_flags: u32,
        ) -> i32;
        fn BCryptDestroyHash(h_hash: *mut c_void) -> i32;
    }

    struct AlgProvider {
        handle: usize,
        object_length: usize,
    }

    fn provider() -> Option<&'static AlgProvider> {
        static ALG: OnceLock<Option<AlgProvider>> = OnceLock::new();
        ALG.get_or_init(|| {
            let mut alg: *mut c_void = std::ptr::null_mut();
            let status = unsafe {
                BCryptOpenAlgorithmProvider(&mut alg, SHA256_ALG_NAME.as_ptr(), std::ptr::null(), 0)
            };
            if status < 0 || alg.is_null() {
                return None;
            }
            let mut obj_len_bytes = [0u8; 4];
            let mut result_len: u32 = 0;
            let prop_status = unsafe {
                BCryptGetProperty(
                    alg,
                    OBJECT_LENGTH_PROP.as_ptr(),
                    obj_len_bytes.as_mut_ptr(),
                    4,
                    &mut result_len,
                    0,
                )
            };
            if prop_status < 0 || result_len != 4 {
                unsafe {
                    let _ = BCryptCloseAlgorithmProvider(alg, 0);
                }
                return None;
            }
            let object_length = u32::from_le_bytes(obj_len_bytes) as usize;
            if object_length == 0 || object_length > 64 * 1024 {
                unsafe {
                    let _ = BCryptCloseAlgorithmProvider(alg, 0);
                }
                return None;
            }
            Some(AlgProvider {
                handle: alg as usize,
                object_length,
            })
        })
        .as_ref()
    }

    struct HashGuard(*mut c_void);
    impl Drop for HashGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    let _ = BCryptDestroyHash(self.0);
                }
            }
        }
    }

    pub(super) fn hash_file(
        file: &mut File,
        buffer: &mut [u8],
    ) -> Option<std::io::Result<[u8; 32]>> {
        let alg = provider()?;
        let mut hash_obj = vec![0u8; alg.object_length];
        let mut hash_handle: *mut c_void = std::ptr::null_mut();
        let status = unsafe {
            BCryptCreateHash(
                alg.handle as *mut c_void,
                &mut hash_handle,
                hash_obj.as_mut_ptr(),
                alg.object_length as u32,
                std::ptr::null_mut(),
                0,
                0,
            )
        };
        if status < 0 || hash_handle.is_null() {
            return None;
        }
        let _guard = HashGuard(hash_handle);
        loop {
            let n = match file.read(buffer) {
                Ok(n) => n,
                Err(err) => return Some(Err(err)),
            };
            if n == 0 {
                break;
            }
            let Ok(chunk_len) = u32::try_from(n) else {
                return None;
            };
            let status = unsafe { BCryptHashData(hash_handle, buffer.as_ptr(), chunk_len, 0) };
            if status < 0 {
                return None;
            }
        }
        let mut digest = [0u8; 32];
        let status =
            unsafe { BCryptFinishHash(hash_handle, digest.as_mut_ptr(), SHA256_DIGEST_LEN, 0) };
        if status < 0 {
            return None;
        }
        Some(Ok(digest))
    }
}

fn format_sha256_hex(digest: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(digest.len() * 2);
    for &byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn file_sha256_software_with_buffer(
    path: &Path,
    buffer: &mut [u8],
) -> Result<String, DeploymentError> {
    let mut file = File::open(path).map_err(|e| io_error(path, e))?;
    let mut digest = Sha256::new();
    loop {
        let n = file.read(buffer).map_err(|e| io_error(path, e))?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    Ok(format_sha256_hex(&digest.finalize()))
}

fn file_sha256_with_buffer(path: &Path, buffer: &mut [u8]) -> Result<String, DeploymentError> {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        let mut file = File::open(path).map_err(|e| io_error(path, e))?;
        if let Some(res) = cng_sha256::hash_file(&mut file, buffer) {
            let digest = res.map_err(|e| io_error(path, e))?;
            return Ok(format_sha256_hex(&digest));
        }
    }
    file_sha256_software_with_buffer(path, buffer)
}

pub fn file_sha256(path: &Path) -> Result<String, DeploymentError> {
    let mut buffer = vec![0u8; HASH_BUFFER_BYTES];
    file_sha256_with_buffer(path, &mut buffer)
}

fn check_hash_with_buffer(
    path: &Path,
    expected: &str,
    buffer: &mut [u8],
) -> Result<(), DeploymentError> {
    if !file_sha256_with_buffer(path, buffer)?.eq_ignore_ascii_case(expected) {
        return Err(DeploymentError::HashMismatch {
            path: path.to_owned(),
        });
    }
    Ok(())
}

fn check_hash(path: &Path, expected: &str) -> Result<(), DeploymentError> {
    let mut buffer = vec![0u8; HASH_BUFFER_BYTES];
    check_hash_with_buffer(path, expected, &mut buffer)
}

fn check_helper_stamp(path: &Path) -> Result<(), DeploymentError> {
    let mut file = File::open(path).map_err(|e| io_error(path, e))?;
    let mut chunk = vec![0u8; HASH_BUFFER_BYTES];
    let mut buffer = Vec::with_capacity(chunk.len() + HELPER_STAMP_LEN);
    let mut tail = Vec::with_capacity(HELPER_STAMP_LEN - 1);
    let lead = HELPER_PREFIX[0];
    loop {
        let n = file.read(&mut chunk).map_err(|e| io_error(path, e))?;
        if n == 0 {
            return Err(DeploymentError::HelperStampMismatch);
        }
        buffer.clear();
        buffer.extend_from_slice(&tail);
        buffer.extend_from_slice(&chunk[..n]);
        let mut pos = 0;
        while pos + HELPER_STAMP_LEN <= buffer.len() {
            let end = buffer.len() - HELPER_STAMP_LEN;
            let Some(rel) = buffer[pos..=end].iter().position(|&b| b == lead) else {
                break;
            };
            let idx = pos + rel;
            let candidate = &buffer[idx..idx + HELPER_STAMP_LEN];
            if candidate.starts_with(HELPER_PREFIX)
                && Sha256::digest(candidate)[..] == HELPER_STAMP_DIGEST
            {
                return Ok(());
            }
            pos = idx + 1;
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
    struct RuntimeCheckWork {
        index: usize,
        size: u64,
        path: PathBuf,
        expected_sha256: String,
    }
    let mut works = Vec::with_capacity(manifest.runtime.len());
    for (index, entry) in manifest.runtime.iter().enumerate() {
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
        let size = fs::metadata(&path).map_err(|e| io_error(&path, e))?.len();
        works.push(RuntimeCheckWork {
            index,
            size,
            path,
            expected_sha256: entry.sha256.clone(),
        });
    }
    if !works.is_empty() {
        let mut schedule: Vec<usize> = (0..works.len()).collect();
        // Longest processing time first so large binaries (`libcef.dll`) start immediately at t=0.
        schedule.sort_by(|&a, &b| works[b].size.cmp(&works[a].size));
        let worker_count = std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1)
            .clamp(1, 8)
            .min(works.len());
        let cursor = AtomicUsize::new(0);
        let outcomes: Vec<OnceLock<Result<(), DeploymentError>>> =
            (0..works.len()).map(|_| OnceLock::new()).collect();
        std::thread::scope(|scope| {
            for _ in 0..worker_count {
                scope.spawn(|| {
                    let mut buffer = vec![0u8; HASH_BUFFER_BYTES];
                    loop {
                        let pos = cursor.fetch_add(1, Ordering::Relaxed);
                        let Some(&work_idx) = schedule.get(pos) else {
                            break;
                        };
                        let work = &works[work_idx];
                        let res = check_hash_with_buffer(
                            &work.path,
                            &work.expected_sha256,
                            &mut buffer,
                        );
                        let _ = outcomes[work.index].set(res);
                    }
                });
            }
        });
        for (i, slot) in outcomes.into_iter().enumerate() {
            match slot.into_inner() {
                Some(res) => res?,
                None => {
                    return Err(DeploymentError::Io {
                        path: works[i].path.clone(),
                        kind: io::ErrorKind::Other,
                    });
                }
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDirGuard(PathBuf);
    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn accelerated_file_sha256_matches_reference_sha2_across_sizes() {
        let root = std::env::temp_dir().join(format!(
            "rust-cef-sha256-test-{}-{}",
            std::process::id(),
            line!()
        ));
        let _guard = TempDirGuard(root.clone());
        fs::create_dir_all(&root).unwrap();

        let sizes = [0usize, 13, 65_536, HASH_BUFFER_BYTES, 3 * 1024 * 1024 + 17];
        for (i, size) in sizes.into_iter().enumerate() {
            let mut payload = vec![0u8; size];
            for (idx, byte) in payload.iter_mut().enumerate() {
                *byte = ((idx * 31 + i * 17) & 0xff) as u8;
            }
            let path = root.join(format!("sample-{i}.bin"));
            fs::write(&path, &payload).unwrap();
            let expected = format!("{:x}", Sha256::digest(&payload));
            let actual = file_sha256(&path).unwrap();
            assert_eq!(actual, expected, "size={size}");
            let mut reusable = vec![0u8; HASH_BUFFER_BYTES];
            let sw = file_sha256_software_with_buffer(&path, &mut reusable).unwrap();
            assert_eq!(sw, expected, "software size={size}");
        }
    }
}
