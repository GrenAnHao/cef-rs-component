//! Bounded owned inbound file payload and phase rules; no file IO or native ref.
use crate::ports::EngineError;
use std::{fmt, path::PathBuf};

pub const MAX_FILE_DROP_PATHS: usize = 32;
pub const MAX_FILE_DROP_PATH_UTF16: usize = 32_767;
pub const MAX_FILE_DROP_TOTAL_UTF16: usize = 65_536;

#[derive(Clone, Eq)]
pub struct FileDropPaths {
    paths: Vec<PathBuf>,
    utf16_units: usize,
}
impl FileDropPaths {
    /// Reject the whole payload, never silently omit an unrepresentable path.
    /// This grants no file access and performs no existence/canonicalization IO.
    pub fn new(paths: Vec<PathBuf>) -> Result<Self, EngineError> {
        let utf16_units = validate_paths(&paths)?;
        Ok(Self { paths, utf16_units })
    }
    /// Validate borrowed platform paths BEFORE making any component-owned copy.
    pub fn copy_from_slice(paths: &[PathBuf]) -> Result<Self, EngineError> {
        let utf16_units = validate_paths(paths)?;
        Ok(Self {
            paths: paths.to_vec(),
            utf16_units,
        })
    }
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }
    /// Associate a later borrowed event with the exact sequence admitted at
    /// Enter. Component-wise `Path` equality can ignore spelling differences.
    /// No normalization, allocations, file lookups, or unbounded path scans.
    pub fn matches_original(&self, paths: &[PathBuf]) -> bool {
        self.paths.len() == paths.len()
            && self.paths.iter().zip(paths).all(|(original, candidate)| {
                original.as_os_str().as_encoded_bytes() == candidate.as_os_str().as_encoded_bytes()
            })
    }

    pub fn validate(&self) -> Result<(), EngineError> {
        if validate_paths(&self.paths)? != self.utf16_units {
            return Err(EngineError::InvalidRequest);
        }
        Ok(())
    }
}
impl PartialEq for FileDropPaths {
    fn eq(&self, other: &Self) -> bool {
        self.matches_original(&other.paths)
    }
}
impl fmt::Debug for FileDropPaths {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileDropPaths")
            .field("count", &self.paths.len())
            .field("utf16_units", &self.utf16_units)
            .finish_non_exhaustive()
    }
}
fn validate_paths(paths: &[PathBuf]) -> Result<usize, EngineError> {
    if paths.is_empty() || paths.len() > MAX_FILE_DROP_PATHS {
        return Err(EngineError::InvalidRequest);
    }
    let mut total = 0usize;
    for path in paths {
        let remaining = (MAX_FILE_DROP_TOTAL_UTF16 - total).min(MAX_FILE_DROP_PATH_UTF16);
        // A valid UTF-8 path needs at most 3 bytes per UTF-16 unit. Check
        // the storage length before scanning externally supplied strings.
        if path.as_os_str().len() > remaining * 3 {
            return Err(EngineError::InvalidRequest);
        }
        let text = path.to_str().ok_or(EngineError::InvalidRequest)?;
        if !path.is_absolute() || text.contains('\0') {
            return Err(EngineError::InvalidRequest);
        }
        let units = text.encode_utf16().take(remaining + 1).count();
        if units == 0 || units > remaining {
            return Err(EngineError::InvalidRequest);
        }
        total += units;
    }
    Ok(total)
}

/// Last issued UI-side stage, NOT a renderer acceptance/drop-completion ACK.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FileDragPhase {
    #[default]
    Idle,
    Entered,
    Over,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileDragStage {
    Enter,
    Over,
    Leave,
    Drop,
}
impl FileDragPhase {
    pub fn transition(self, stage: FileDragStage) -> Result<Self, EngineError> {
        use FileDragPhase::*;
        use FileDragStage as Stage;
        match (self, stage) {
            (Idle, Stage::Enter) => Ok(Entered),
            (Entered | Over, Stage::Over) => Ok(Over),
            (_, Stage::Leave) | (Over, Stage::Drop) => Ok(Idle),
            _ => Err(EngineError::InvalidRequest),
        }
    }
    pub fn is_active(self) -> bool {
        self != Self::Idle
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn absolute(name: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:/owned/{name}"))
        } else {
            PathBuf::from(format!("/owned/{name}"))
        }
    }
    #[test]
    fn borrowed_platform_paths_validate_whole_payload_before_owned_copy() {
        let paths = vec![absolute("原始😀.txt")];
        let owned = FileDropPaths::copy_from_slice(&paths).unwrap();
        assert_eq!(owned.paths(), paths.as_slice());
        assert_eq!(owned.validate(), Ok(()));
        assert_eq!(
            FileDropPaths::copy_from_slice(&[]),
            Err(EngineError::InvalidRequest)
        );
        assert_eq!(
            FileDropPaths::copy_from_slice(&[absolute("ok"), PathBuf::from("relative")]),
            Err(EngineError::InvalidRequest)
        );
        assert_eq!(
            FileDropPaths::copy_from_slice(&vec![absolute("x"); MAX_FILE_DROP_PATHS + 1]),
            Err(EngineError::InvalidRequest)
        );
        assert_eq!(
            FileDropPaths::copy_from_slice(&[absolute(&"x".repeat(MAX_FILE_DROP_PATH_UTF16))]),
            Err(EngineError::InvalidRequest)
        );
        assert!(!format!("{owned:?}").contains("原始"));
    }
    #[test]
    fn inbound_paths_are_whole_payload_bounded_utf16_and_debug_redacted() {
        let good = FileDropPaths::new(vec![absolute("private-中😀.txt")]).unwrap();
        assert_eq!(good.validate(), Ok(()));
        assert_eq!(good.paths().len(), 1);
        assert!(!format!("{good:?}").contains("private"));
        assert!(!format!("{good:?}").contains("owned"));
        let mut event = crate::input::InputEvent {
            epoch: 7,
            kind: crate::input::InputKind::FileDragEnter {
                paths: good,
                x: 10,
                y: 20,
                modifiers: crate::ports::MouseModifiers::default(),
            },
        };
        assert_eq!(event.validate(), Ok(()));
        assert!(!format!("{event:?}").contains("private"));
        if let crate::input::InputKind::FileDragEnter { x, .. } = &mut event.kind {
            *x = -1;
        }
        assert_eq!(event.validate(), Err(EngineError::InvalidRequest));
        for paths in [
            vec![],
            vec![PathBuf::from("relative.txt")],
            vec![absolute("a"), PathBuf::from("bad")],
            vec![absolute("nul\0.txt")],
            vec![absolute("x"); MAX_FILE_DROP_PATHS + 1],
            vec![absolute(&"😀".repeat(MAX_FILE_DROP_PATH_UTF16 / 2))],
            vec![absolute(&"x".repeat(22_000)); 3],
        ] {
            assert_eq!(FileDropPaths::new(paths), Err(EngineError::InvalidRequest));
        }
    }
    #[test]
    fn original_path_identity_does_not_alias_lexical_components() {
        let first = absolute("一/文件😀.txt");
        let second = absolute("尾.txt");
        let payload = FileDropPaths::copy_from_slice(&[first.clone(), second.clone()]).unwrap();
        let alias = absolute("一//./文件😀.txt");
        assert_eq!(first, alias, "Path equality loses original spelling");
        assert!(payload.matches_original(&[first.clone(), second.clone()]));
        assert!(!payload.matches_original(&[alias.clone(), second.clone()]));
        assert!(!payload.matches_original(&[second.clone(), first.clone()]));
        assert!(!payload.matches_original(&[first.clone()]));
        assert!(!payload.matches_original(&[first, second.clone(), second.clone()]));
        assert_ne!(payload, FileDropPaths::new(vec![alias, second]).unwrap());
    }

    #[test]
    fn bounded_utf16_paths_accept_exact_caps_and_refuse_one_extra_unit() {
        let prefix = if cfg!(windows) { "C:/" } else { "/" };
        let path = |units, piece: &str| {
            PathBuf::from(format!("{prefix}{}", piece.repeat(units - prefix.len())))
        };
        let at_limit = path(MAX_FILE_DROP_PATH_UTF16, "中");
        let next = path(MAX_FILE_DROP_PATH_UTF16 - 1, "x");
        let last = path(3, "x");
        assert_eq!(
            FileDropPaths::copy_from_slice(&[at_limit.clone(), next.clone(), last])
                .unwrap()
                .utf16_units,
            MAX_FILE_DROP_TOTAL_UTF16
        );
        assert_eq!(
            FileDropPaths::copy_from_slice(&[at_limit, next, path(4, "x")]),
            Err(EngineError::InvalidRequest)
        );
        let with_pair = PathBuf::from(format!(
            "{prefix}{}😀",
            "x".repeat(MAX_FILE_DROP_PATH_UTF16 - prefix.len() - 2)
        ));
        assert_eq!(
            FileDropPaths::copy_from_slice(&[with_pair])
                .unwrap()
                .utf16_units,
            MAX_FILE_DROP_PATH_UTF16
        );
        let mut above = path(MAX_FILE_DROP_PATH_UTF16, "x").into_os_string();
        above.push("x");
        assert_eq!(
            FileDropPaths::copy_from_slice(&[PathBuf::from(above)]),
            Err(EngineError::InvalidRequest)
        );
        assert_eq!(
            FileDropPaths::copy_from_slice(&[path(MAX_FILE_DROP_PATH_UTF16 + 1, "中")]),
            Err(EngineError::InvalidRequest)
        );
    }

    #[cfg(windows)]
    #[test]
    fn malformed_windows_borrowed_path_is_refused_without_replacement() {
        use std::{ffi::OsString, os::windows::ffi::OsStringExt};
        let path = PathBuf::from(OsString::from_wide(&[67, 58, 92, 0xd800, 46, 116]));
        assert!(path.is_absolute());
        assert!(path.to_str().is_none());
        assert_eq!(
            FileDropPaths::copy_from_slice(&[absolute("最先.txt"), path]),
            Err(EngineError::InvalidRequest)
        );
    }

    #[test]
    fn inbound_file_phases_require_enter_and_over_and_cancel_is_idempotent() {
        use FileDragPhase::*;
        use FileDragStage as Stage;
        assert_eq!(
            Idle.transition(Stage::Over),
            Err(EngineError::InvalidRequest)
        );
        assert_eq!(
            Idle.transition(Stage::Drop),
            Err(EngineError::InvalidRequest)
        );
        assert_eq!(Idle.transition(Stage::Leave), Ok(Idle));
        let entered = Idle.transition(Stage::Enter).unwrap();
        assert_eq!(entered, Entered);
        assert_eq!(
            entered.transition(Stage::Enter),
            Err(EngineError::InvalidRequest)
        );
        assert_eq!(
            entered.transition(Stage::Drop),
            Err(EngineError::InvalidRequest)
        );
        let over = entered.transition(Stage::Over).unwrap();
        assert_eq!(over.transition(Stage::Over), Ok(Over));
        assert_eq!(over.transition(Stage::Drop), Ok(Idle));
        for active in [Entered, Over] {
            let cancelled = active.transition(Stage::Leave).unwrap();
            assert_eq!(cancelled.transition(Stage::Leave), Ok(Idle));
            assert_eq!(
                cancelled.transition(Stage::Drop),
                Err(EngineError::InvalidRequest)
            );
        }
    }
}
