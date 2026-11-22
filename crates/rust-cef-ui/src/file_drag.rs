//! UI-local gesture binding, not an OS drag ID or renderer acceptance token.
use rust_cef_core::{
    file_drop::{FileDragPhase, FileDropPaths},
    input::InputSnapshot,
    ports::EngineError,
};
use std::path::PathBuf;

pub struct FileDragSession {
    pub serial: u64,
    pub epoch: u64,
    pub cancelled: bool,
    paths: FileDropPaths,
    phase: FileDragPhase,
}
impl FileDragSession {
    pub fn new(serial: u64, epoch: u64, paths: FileDropPaths) -> Self {
        Self {
            serial,
            epoch,
            cancelled: false,
            paths,
            phase: FileDragPhase::Entered,
        }
    }
    pub fn matches_paths(&self, paths: &[PathBuf]) -> bool {
        self.paths.matches_original(paths)
    }
    pub fn ready(&self, snapshot: &InputSnapshot, dropping: bool) -> Result<(), EngineError> {
        if self.cancelled
            || snapshot.epoch != self.epoch
            || snapshot.cleanup_pending
            || snapshot.cleanup_failed
            || snapshot.native_execution_unconfirmed
            || snapshot.inflight != 0
        {
            return Err(EngineError::NotReady);
        }
        if snapshot.file_drag_phase != self.phase
            || !snapshot.file_drag_active
            || (dropping && self.phase != FileDragPhase::Over)
        {
            return Err(EngineError::InvalidRequest);
        }
        Ok(())
    }
    pub fn over_admitted(&mut self) {
        self.phase = FileDragPhase::Over;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn active_file_session_rejects_replacement_path_spelling() {
        let original = PathBuf::from("C:/owned/夹/原始😀.txt");
        let alias = PathBuf::from("C:/owned/夹//./原始😀.txt");
        assert_eq!(original, alias);
        let mut session = FileDragSession::new(
            12,
            7,
            FileDropPaths::copy_from_slice(std::slice::from_ref(&original)).unwrap(),
        );
        assert!(!session.matches_paths(std::slice::from_ref(&alias)));
        session.over_admitted();
        assert!(!session.matches_paths(&[alias]));
        assert!(session.matches_paths(&[original]));
    }

    #[test]
    fn original_file_session_requires_confirmed_same_epoch_over_and_cancel_is_terminal() {
        let paths = vec![PathBuf::from("C:/owned/中文😀.txt")];
        let mut session =
            FileDragSession::new(12, 7, FileDropPaths::copy_from_slice(&paths).unwrap());
        let mut snapshot = InputSnapshot {
            epoch: 7,
            admitted: 1,
            executed: 1,
            rejected: 0,
            inflight: 0,
            native_execution_unconfirmed: false,
            pressed_buttons: 0,
            pressed_keys: 0,
            composition_active: false,
            file_drag_active: true,
            file_drag_phase: FileDragPhase::Entered,
            cleanup_pending: false,
            cleanup_failed: false,
            last_native_error: None,
            accepts_text: false,
        };
        assert!(session.matches_paths(&paths));
        assert!(!session.matches_paths(&[PathBuf::from("C:/owned/other.txt")]));
        assert_eq!(session.ready(&snapshot, false), Ok(()));
        assert_eq!(
            session.ready(&snapshot, true),
            Err(EngineError::InvalidRequest)
        );
        session.over_admitted();
        assert_eq!(
            session.ready(&snapshot, true),
            Err(EngineError::InvalidRequest)
        );
        snapshot.file_drag_phase = FileDragPhase::Over;
        assert_eq!(session.ready(&snapshot, true), Ok(()));
        snapshot.inflight = 1;
        assert_eq!(session.ready(&snapshot, true), Err(EngineError::NotReady));
        snapshot.inflight = 0;
        snapshot.native_execution_unconfirmed = true;
        assert_eq!(session.ready(&snapshot, true), Err(EngineError::NotReady));
        snapshot.native_execution_unconfirmed = false;
        snapshot.epoch = 8;
        assert_eq!(session.ready(&snapshot, true), Err(EngineError::NotReady));
        snapshot.epoch = 7;
        snapshot.cleanup_pending = true;
        assert_eq!(session.ready(&snapshot, true), Err(EngineError::NotReady));
        snapshot.cleanup_pending = false;
        session.cancelled = true;
        assert_eq!(session.ready(&snapshot, true), Err(EngineError::NotReady));
    }
}
