//! Windows storage layout and cooperative root lease. Reparse checks are not a
//! sandbox: another process can replace filesystem entries between checks.
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::windows::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf, Prefix},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoragePathError {
    NotAbsolute,
    Traversal,
    Reparse,
    NotDirectChild,
    ReservedDefault,
    Overlap,
    InvalidUnicode,
    LeaseInUse,
    Io { code: Option<i32> },
}
impl std::fmt::Display for StoragePathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "storage path error: {self:?}")
    }
}
impl std::error::Error for StoragePathError {}
fn io_error(e: io::Error) -> StoragePathError {
    StoragePathError::Io {
        code: e.raw_os_error(),
    }
}

fn validate(path: &Path) -> Result<(), StoragePathError> {
    if path.to_str().is_none() {
        return Err(StoragePathError::InvalidUnicode);
    }
    if !path.is_absolute() {
        return Err(StoragePathError::NotAbsolute);
    }
    match path.components().next() {
        Some(Component::Prefix(p))
            if matches!(
                p.kind(),
                Prefix::Disk(_)
                    | Prefix::UNC(_, _)
                    | Prefix::VerbatimDisk(_)
                    | Prefix::VerbatimUNC(_, _)
            ) => {}
        _ => return Err(StoragePathError::NotAbsolute),
    }
    // Inspect both the actual spelling and parsed components. Verbatim Windows
    // paths treat separators specially; neither spelling may hide traversal.
    if path
        .to_str()
        .expect("Unicode checked above")
        .split(['\\', '/'])
        .any(|part| part == "..")
        || path.components().any(|c| c == Component::ParentDir)
    {
        return Err(StoragePathError::Traversal);
    }
    Ok(())
}
fn check_existing(path: &Path) -> Result<bool, StoragePathError> {
    match fs::symlink_metadata(path) {
        Ok(m) => {
            if m.file_attributes() & 0x400 != 0 {
                return Err(StoragePathError::Reparse);
            }
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(io_error(e)),
    }
}
// Check every existing component before canonicalizing, then project the
// missing suffix onto the real ancestor (including long-name expansion).
fn projected(path: &Path) -> Result<PathBuf, StoragePathError> {
    validate(path)?;
    let mut current = PathBuf::new();
    let mut missing = Vec::new();
    let mut found_missing = false;
    for component in path.components() {
        current.push(component.as_os_str());
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        if found_missing || !check_existing(&current)? {
            found_missing = true;
            missing.push(component.as_os_str().to_owned());
        }
    }
    let mut ancestor = current;
    for _ in &missing {
        ancestor.pop();
    }
    let mut real = fs::canonicalize(ancestor).map_err(io_error)?;
    for part in missing {
        real.push(part);
    }
    if real.to_str().is_none() {
        return Err(StoragePathError::InvalidUnicode);
    }
    Ok(real)
}
pub fn prepare_directory(path: &Path) -> Result<PathBuf, StoragePathError> {
    let projected = projected(path)?;
    fs::create_dir_all(&projected).map_err(io_error)?;
    // Recheck the original spelling as well as the physical spelling.
    let real = projected_path_after_creation(path)?;
    projected_path_after_creation(&real)
}
fn projected_path_after_creation(path: &Path) -> Result<PathBuf, StoragePathError> {
    let real = projected(path)?;
    fs::canonicalize(real).map_err(io_error)
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CompareStringOrdinal(
        a: *const u16,
        a_len: i32,
        b: *const u16,
        b_len: i32,
        ignore_case: i32,
    ) -> i32;
}
pub fn paths_equal(a: &Path, b: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    let a: Vec<u16> = a.as_os_str().encode_wide().collect();
    let b: Vec<u16> = b.as_os_str().encode_wide().collect();
    let (Ok(al), Ok(bl)) = (i32::try_from(a.len()), i32::try_from(b.len())) else {
        return false;
    };
    unsafe { CompareStringOrdinal(a.as_ptr(), al, b.as_ptr(), bl, 1) == 2 }
}
fn within(path: &Path, base: &Path) -> bool {
    path.ancestors().any(|ancestor| paths_equal(ancestor, base))
}

pub struct RootLease {
    root: PathBuf,
    file: Option<File>,
}
impl RootLease {
    pub fn acquire(root: &Path) -> Result<Self, StoragePathError> {
        let root = prepare_directory(root)?;
        let path = root.join(".rust-cef-gpui.lease");
        check_existing(&path).map_err(|e| match e {
            StoragePathError::Io {
                code: Some(32 | 33),
            } => StoragePathError::LeaseInUse,
            other => other,
        })?;
        // OPEN_REPARSE_POINT prevents following a lease-file link even if it
        // was swapped after the metadata check.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .share_mode(0)
            .custom_flags(0x0020_0000)
            .open(&path)
            .map_err(|e| match e.raw_os_error() {
                Some(32 | 33) => StoragePathError::LeaseInUse,
                _ => io_error(e),
            })?;
        if file.metadata().map_err(io_error)?.file_attributes() & 0x400 != 0 {
            return Err(StoragePathError::Reparse);
        }
        Ok(Self {
            root,
            file: Some(file),
        })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn is_held(&self) -> bool {
        self.file.is_some()
    }
    pub fn release(&mut self) {
        self.file.take();
    }
    pub fn abandon(&mut self) {
        if let Some(file) = self.file.take() {
            std::mem::forget(file);
        }
    }
}

pub struct ProfilePaths {
    root: PathBuf,
    entries: Vec<(String, PathBuf)>,
}
impl ProfilePaths {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            entries: Vec::new(),
        }
    }
    pub fn register(&mut self, name: &str, path: &Path) -> Result<PathBuf, StoragePathError> {
        let candidate = projected(path)?;
        self.check(name, &candidate)?;
        let real = prepare_directory(path)?;
        self.check(name, &real)?;
        if !self
            .entries
            .iter()
            .any(|(n, p)| n == name && paths_equal(p, &real))
        {
            self.entries.push((name.to_owned(), real.clone()));
        }
        Ok(real)
    }
    fn check(&self, name: &str, path: &Path) -> Result<(), StoragePathError> {
        if paths_equal(path, &self.root)
            || ["Default", "System Profile", "Guest Profile"]
                .iter()
                .any(|name| within(path, &self.root.join(name)))
        {
            return Err(StoragePathError::ReservedDefault);
        }
        // CEF 146 Chrome runtime accepts only direct children of user_data_dir.
        // A nested cache path silently falls back to OTR, so reject it up front.
        if !path
            .parent()
            .is_some_and(|parent| paths_equal(parent, &self.root))
        {
            return Err(StoragePathError::NotDirectChild);
        }
        for (other_name, other_path) in &self.entries {
            if other_name == name && paths_equal(other_path, path) {
                continue;
            }
            if other_name == name || within(path, other_path) || within(other_path, path) {
                return Err(StoragePathError::Overlap);
            }
        }
        Ok(())
    }
}
