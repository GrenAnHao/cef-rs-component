use rust_cef_core::{
    contracts::{ProfileConfig, ProfileHandle, ProfileStorage, RuntimeGeneration},
    profiles::{ProfileCatalog, ProfileError, ProfileState},
};

fn catalog() -> ProfileCatalog {
    ProfileCatalog::new(RuntimeGeneration::new(31), ProfileStorage::Memory)
}
fn memory(name: &str) -> ProfileConfig {
    ProfileConfig {
        name: name.into(),
        storage: ProfileStorage::Memory,
    }
}
fn ready_memory_default(p: &mut ProfileCatalog) {
    let id = p.default_handle();
    p.create_returned(id, true).unwrap();
    p.context_initialized(id).unwrap();
}

#[test]
fn none_uses_default() {
    let mut p = catalog();
    assert_eq!(p.select(None), Err(ProfileError::NotReady));
    ready_memory_default(&mut p);
    let first = p.select(None).unwrap();
    assert_eq!(first, p.default_handle());
    assert_eq!(p.select(None), p.select(Some(first)));
    assert_eq!(p.config(first).unwrap().storage, ProfileStorage::Memory);
}

#[test]
fn default_ready_without_context_handler() {
    // Only Persistent uses CEF's global context. Memory must not inherit the
    // Chrome global's disk-backed profile despite an empty requested cache path.
    let mut p = ProfileCatalog::new(
        RuntimeGeneration::new(31),
        ProfileStorage::Persistent {
            directory: std::env::temp_dir(),
        },
    );
    let default = p.default_handle();
    assert_eq!(p.state(default), Ok(ProfileState::Pending));
    p.default_context_ready().unwrap();
    p.default_context_ready().unwrap();
    assert_eq!(p.state(default), Ok(ProfileState::Ready));
    assert_eq!(
        p.context_initialized(default),
        Err(ProfileError::InvalidTransition)
    );
    assert_eq!(
        p.create_returned(default, true),
        Err(ProfileError::InvalidTransition)
    );
    p.default_context_failed();
    p.default_context_ready().unwrap();
    assert_eq!(p.state(default), Ok(ProfileState::Failed));
}

#[test]
fn memory_default_waits_for_its_own_factory_and_handler() {
    for early in [true, false] {
        let mut p = catalog();
        let id = p.default_handle();
        assert_eq!(
            p.default_context_ready(),
            Err(ProfileError::InvalidTransition)
        );
        if early {
            p.context_initialized(id).unwrap();
        } else {
            p.create_returned(id, true).unwrap();
        }
        assert_eq!(p.state(id), Ok(ProfileState::Pending));
        assert_eq!(p.select(None), Err(ProfileError::NotReady));
        if early {
            p.create_returned(id, true).unwrap();
        } else {
            p.context_initialized(id).unwrap();
        }
        assert_eq!(p.select(None), Ok(id));
    }
    let mut failed = catalog();
    let id = failed.default_handle();
    failed.default_context_failed();
    assert_eq!(
        failed.create_returned(id, true),
        Err(ProfileError::InvalidTransition)
    );
    failed.context_initialized(id).unwrap();
    assert_eq!(failed.state(id), Ok(ProfileState::Failed));
}

#[test]
fn same_profile_reuses_context() {
    let mut p = catalog();
    let a = p.reserve(memory("P1")).unwrap();
    let b = p.reserve(memory("P1")).unwrap();
    let other = p.reserve(memory("P2")).unwrap();
    assert!(a.is_new);
    assert!(!b.is_new);
    assert_eq!(a.handle, b.handle);
    assert_eq!(p.named_handle("P1"), Some(a.handle));
    assert_eq!(p.named_handle("missing"), None);
    assert_ne!(a.handle, other.handle);
    assert_ne!(a.handle, p.default_handle());
    p.create_returned(a.handle, true).unwrap();
    p.context_initialized(a.handle).unwrap();
    assert_eq!(p.select(Some(b.handle)), Ok(a.handle));
    assert_eq!(p.reserve(memory("P1")).unwrap().handle, a.handle);
    let conflicting = ProfileConfig {
        name: "P1".into(),
        storage: ProfileStorage::Persistent {
            directory: std::env::temp_dir(),
        },
    };
    assert_eq!(p.reserve(conflicting), Err(ProfileError::ConfigConflict));
}

#[test]
fn foreign_handle_rejected() {
    let mut p = catalog();
    ready_memory_default(&mut p);
    let foreign = ProfileHandle::new(RuntimeGeneration::new(32), 0);
    assert_eq!(p.select(Some(foreign)), Err(ProfileError::WrongRuntime));
    assert_eq!(p.state(foreign), Err(ProfileError::WrongRuntime));
    assert_eq!(
        p.context_initialized(foreign),
        Err(ProfileError::WrongRuntime)
    );
    assert_eq!(
        p.create_returned(foreign, true),
        Err(ProfileError::WrongRuntime)
    );
    assert_eq!(p.config(foreign), Err(ProfileError::WrongRuntime));
    assert_eq!(
        p.select(Some(ProfileHandle::new(RuntimeGeneration::new(31), 999))),
        Err(ProfileError::UnknownProfile)
    );
}

#[test]
fn pending_profile_is_not_ready() {
    let mut p = catalog();
    let id = p.reserve(memory("P1")).unwrap().handle;
    assert_eq!(p.select(Some(id)), Err(ProfileError::NotReady));
    p.create_returned(id, true).unwrap();
    assert_eq!(p.state(id), Ok(ProfileState::Pending));
    p.context_initialized(id).unwrap();
    assert_eq!(p.select(Some(id)), Ok(id));
}

#[test]
fn callback_before_factory_return_is_merged() {
    let mut p = catalog();
    let id = p.reserve(memory("early")).unwrap().handle;
    p.context_initialized(id).unwrap();
    assert_eq!(p.select(Some(id)), Err(ProfileError::NotReady));
    p.create_returned(id, true).unwrap();
    p.context_initialized(id).unwrap();
    assert_eq!(p.state(id), Ok(ProfileState::Ready));
    assert_eq!(
        p.create_returned(id, true),
        Err(ProfileError::InvalidTransition)
    );
}

#[test]
fn failed_creation_never_revives_or_falls_back() {
    let mut p = catalog();
    ready_memory_default(&mut p);
    for (name, early) in [("early-failed", true), ("late-failed", false)] {
        let id = p.reserve(memory(name)).unwrap().handle;
        if early {
            p.context_initialized(id).unwrap();
        }
        p.create_returned(id, false).unwrap();
        p.context_initialized(id).unwrap();
        assert_eq!(p.state(id), Ok(ProfileState::Failed));
        assert_eq!(p.select(Some(id)), Err(ProfileError::NotReady));
        assert!(!p.reserve(memory(name)).unwrap().is_new);
    }
    p.default_context_failed();
    assert_eq!(
        p.default_context_ready(),
        Err(ProfileError::InvalidTransition)
    );
    assert_eq!(p.state(p.default_handle()), Ok(ProfileState::Failed));
    assert_eq!(p.select(None), Err(ProfileError::NotReady));
}

#[test]
fn invalid_and_reserved_names_do_not_allocate() {
    let mut p = catalog();
    for name in ["", " ", " P1", "P1 ", "default", "DEFAULT"] {
        assert_eq!(p.reserve(memory(name)), Err(ProfileError::InvalidName));
    }
    assert_eq!(p.reserve(memory("P1")).unwrap().handle.local(), 1);
}

#[test]
fn policy_preparation_joins_native_readiness_without_changing_context_identity() {
    for global in [false, true] {
        for preparation_first in [false, true] {
            let storage = if global {
                ProfileStorage::Persistent {
                    directory: std::env::temp_dir(),
                }
            } else {
                ProfileStorage::Memory
            };
            let mut p = ProfileCatalog::new(RuntimeGeneration::new(31), storage);
            p.require_preparation().unwrap();
            let default = p.default_handle();
            let named = p.reserve(memory("prepared")).unwrap().handle;
            for handle in [default, named] {
                if preparation_first {
                    p.preparation_complete(handle, true).unwrap();
                }
                assert_eq!(p.select(Some(handle)), Err(ProfileError::NotReady));
                assert!(!p.native_initialized(handle).unwrap());
                if global && handle == default {
                    p.default_context_ready().unwrap();
                } else {
                    p.context_initialized(handle).unwrap();
                    p.create_returned(handle, true).unwrap();
                }
                assert!(p.native_initialized(handle).unwrap());
                if !preparation_first {
                    assert_eq!(p.state(handle), Ok(ProfileState::Pending));
                    p.preparation_complete(handle, true).unwrap();
                }
                assert_eq!(p.select(Some(handle)), Ok(handle));
            }
            assert_eq!(p.default_handle(), default);
            assert_eq!(p.named_handle("prepared"), Some(named));
            assert_eq!(
                p.require_preparation(),
                Err(ProfileError::InvalidTransition)
            );
        }
    }
}

#[test]
fn policy_preparation_failure_is_terminal_even_after_late_native_completion() {
    let mut p = catalog();
    p.require_preparation().unwrap();
    let id = p.default_handle();
    p.context_initialized(id).unwrap();
    p.create_returned(id, true).unwrap();
    p.preparation_complete(id, false).unwrap();
    p.context_initialized(id).unwrap();
    assert_eq!(
        p.preparation_complete(id, true),
        Err(ProfileError::InvalidTransition)
    );
    assert_eq!(p.state(id), Ok(ProfileState::Failed));
    assert_eq!(p.select(None), Err(ProfileError::NotReady));
    assert!(!p.native_initialized(id).unwrap());
}
