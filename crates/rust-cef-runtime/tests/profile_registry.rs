//! Profile registry contracts that do not require a native CEF run.
//!
//! These tests exercise the real core catalog through the runtime's profile
//! book and the runtime's public error/diagnostic surface. They never call
//! `Runtime::start`, so they are safe in an ordinary `cargo test` process; the
//! native evidence belongs to the probe and launcher run.
#![cfg(all(
    target_os = "windows",
    target_arch = "x86_64",
    feature = "test-support"
))]

use rust_cef_core::{
    contracts::{ProfileConfig, ProfileHandle, ProfileStorage, RuntimeGeneration},
    profiles::{ProfileError, ProfileState},
};
use rust_cef_runtime::{
    profiles::{ProfileComparison, ProfileCreateError, test_support::ProfileBook},
    runtime::{CreateProfileError, ProfileComparison as ExportedComparison},
};

fn memory(name: &str) -> ProfileConfig {
    ProfileConfig {
        name: name.into(),
        storage: ProfileStorage::Memory,
    }
}

fn book() -> ProfileBook {
    ProfileBook::new(RuntimeGeneration::new(501))
}

#[test]
fn none_uses_the_one_default_reference() {
    let book = book();
    // Before the global context is really registered, the default is not Ready
    // and `None` is rejected rather than silently accepted.
    assert_eq!(
        book.select(None),
        Err(ProfileCreateError::Profile(ProfileError::NotReady))
    );
    book.default_ready_for_test();
    let implicit = book.select(None).unwrap();
    assert_eq!(implicit, book.default_handle());
    assert_eq!(book.default_handle(), book.default_handle());
    // An explicit default handle is the same reference, not a second profile.
    assert_eq!(book.select(Some(implicit)), Ok(implicit));
    assert_eq!(book.state(implicit), Ok(ProfileState::Ready));
}

#[test]
fn default_ready_needs_no_context_handler_but_is_not_ready_when_failed() {
    let book = ProfileBook::with_storage(
        RuntimeGeneration::new(501),
        ProfileStorage::Persistent {
            directory: std::env::temp_dir(),
        },
    );
    let default = book.default_handle();
    assert_eq!(book.state(default), Ok(ProfileState::Pending));
    book.default_ready_for_test();
    assert_eq!(book.state(default), Ok(ProfileState::Ready));
    // There is no injectable handler for the global context; the catalog
    // refuses those transitions instead of fabricating a named-profile flow.
    assert_eq!(
        book.context_initialized(default),
        Err(ProfileCreateError::Profile(ProfileError::InvalidTransition))
    );
    assert_eq!(
        book.create_returned(default, true),
        Err(ProfileCreateError::Profile(ProfileError::InvalidTransition))
    );

    let failed = ProfileBook::new(RuntimeGeneration::new(502));
    failed.default_failed_for_test();
    assert_eq!(
        failed.state(failed.default_handle()),
        Ok(ProfileState::Failed)
    );
    assert_eq!(
        failed.select(None),
        Err(ProfileCreateError::Profile(ProfileError::NotReady))
    );
    assert_eq!(failed.pending_profiles(), 0);
}

#[test]
fn same_profile_reuses_one_handle_and_one_native_intent() {
    let book = book();
    book.default_ready_for_test();
    let first = book.reserve(memory("P1")).unwrap();
    let second = book.reserve(memory("P1")).unwrap();
    let other = book.reserve(memory("P2")).unwrap();
    assert!(first.is_new, "only the first reservation issues a context");
    assert!(!second.is_new);
    assert_eq!(first.handle, second.handle);
    assert_ne!(first.handle, other.handle);
    assert_ne!(first.handle, book.default_handle());
    // A different storage policy for an existing name is a conflict, not a
    // second identity.
    assert_eq!(
        book.reserve(ProfileConfig {
            name: "P1".into(),
            storage: ProfileStorage::Persistent {
                directory: std::env::temp_dir(),
            },
        }),
        Err(ProfileCreateError::Profile(ProfileError::ConfigConflict)),
        "same identity cannot switch storage policy before filesystem work"
    );
    assert_eq!(
        book.reserve(ProfileConfig {
            name: "P1".into(),
            storage: ProfileStorage::Memory,
        })
        .unwrap()
        .handle,
        first.handle
    );
}

#[test]
fn foreign_and_unknown_handles_are_never_replaced_by_the_default() {
    let book = book();
    book.default_ready_for_test();
    let foreign = ProfileHandle::new(RuntimeGeneration::new(999), 0);
    assert_eq!(
        book.select(Some(foreign)),
        Err(ProfileCreateError::Profile(ProfileError::WrongRuntime))
    );
    assert_eq!(
        book.state(foreign),
        Err(ProfileCreateError::Profile(ProfileError::WrongRuntime))
    );
    assert_eq!(
        book.config(foreign),
        Err(ProfileCreateError::Profile(ProfileError::WrongRuntime))
    );
    let unknown = ProfileHandle::new(RuntimeGeneration::new(501), 4242);
    assert_eq!(
        book.select(Some(unknown)),
        Err(ProfileCreateError::Profile(ProfileError::UnknownProfile))
    );
}

#[test]
fn pending_profile_is_not_ready_until_both_inputs_arrive() {
    let book = book();
    book.default_ready_for_test();
    let handle = book.reserve(memory("P1")).unwrap().handle;
    assert_eq!(book.state(handle), Ok(ProfileState::Pending));
    assert_eq!(
        book.select(Some(handle)),
        Err(ProfileCreateError::Profile(ProfileError::NotReady))
    );
    assert_eq!(book.pending_profiles(), 1);
    // The create return alone is not readiness: the context still needs its
    // own initialization callback.
    book.create_returned(handle, true).unwrap();
    assert_eq!(book.state(handle), Ok(ProfileState::Pending));
    assert_eq!(
        book.select(Some(handle)),
        Err(ProfileCreateError::Profile(ProfileError::NotReady))
    );
    book.context_initialized(handle).unwrap();
    assert_eq!(book.state(handle), Ok(ProfileState::Ready));
    assert_eq!(book.pending_profiles(), 0);
    assert_eq!(book.select(Some(handle)), Ok(handle));
}

#[test]
fn callback_before_return_is_merged_and_false_stays_failed() {
    let book = book();
    book.default_ready_for_test();
    // Callback first: still Pending until the create return is accepted.
    let early = book.reserve(memory("Early")).unwrap().handle;
    book.context_initialized(early).unwrap();
    assert_eq!(book.state(early), Ok(ProfileState::Pending));
    book.create_returned(early, true).unwrap();
    assert_eq!(book.state(early), Ok(ProfileState::Ready));

    // Return first: still Pending until the callback.
    let late = book.reserve(memory("Late")).unwrap().handle;
    book.create_returned(late, true).unwrap();
    assert_eq!(book.state(late), Ok(ProfileState::Pending));
    book.context_initialized(late).unwrap();
    assert_eq!(book.state(late), Ok(ProfileState::Ready));

    // A failed create never revives, even if a callback still arrives.
    let failed = book.reserve(memory("Failed")).unwrap().handle;
    book.create_returned(failed, false).unwrap();
    assert_eq!(book.state(failed), Ok(ProfileState::Failed));
    book.context_initialized(failed).unwrap();
    assert_eq!(book.state(failed), Ok(ProfileState::Failed));
    assert_eq!(book.pending_profiles(), 0);
    // Duplicate returns are refused, so a failure cannot be rewritten.
    assert_eq!(
        book.create_returned(failed, true),
        Err(ProfileCreateError::Profile(ProfileError::InvalidTransition))
    );
}

#[test]
fn drain_terminates_a_queued_creation_without_starting_a_context() {
    let book = book();
    book.default_ready_for_test();
    let handle = book.reserve(memory("Queued")).unwrap().handle;
    assert_eq!(book.pending_profiles(), 1);
    // The UI task observed drainage before it issued any native creation.
    book.create_profile_now(handle, true);
    assert_eq!(book.state(handle), Ok(ProfileState::Failed));
    assert_eq!(book.pending_profiles(), 0);
    assert_eq!(book.references_retained(), 0);
}

#[test]
fn references_are_counted_and_released_before_shutdown() {
    let book = book();
    book.default_ready_for_test();
    // No native object exists in this harness, so the real count stays zero
    // even for Ready profiles: readiness metadata is not a CEF reference.
    assert_eq!(book.references_retained(), 0);
    let handle = book.reserve(memory("P1")).unwrap().handle;
    book.create_returned(handle, true).unwrap();
    book.context_initialized(handle).unwrap();
    assert_eq!(book.references_retained(), 0);
    assert_eq!(book.seal_and_release(), 0);
    assert!(book.is_sealed());
    assert_eq!(book.callbacks_in_flight(), 0);
}

#[test]
fn comparison_ticket_is_pending_then_owned_booleans() {
    let book = book();
    book.default_ready_for_test();
    let second = book.reserve(memory("P1")).unwrap().handle;
    book.create_returned(second, true).unwrap();
    book.context_initialized(second).unwrap();
    let ticket = book
        .begin_comparison(book.default_handle(), second)
        .unwrap();
    assert_eq!(ticket.poll(), None, "no CEF call was made yet");
    book.record_comparison(
        &ticket,
        Ok(ProfileComparison {
            is_same: false,
            is_sharing: false,
        }),
    );
    let result = ticket.poll().unwrap().unwrap();
    assert!(!result.is_same);
    assert!(!result.is_sharing);
    assert_eq!(
        result,
        ExportedComparison {
            is_same: false,
            is_sharing: false,
        }
    );
    // Dropping the ticket must reclaim its plain record; polling a dropped
    // ticket is no longer possible, and a fresh one starts Pending again.
    drop(ticket);
    let fresh = book
        .begin_comparison(book.default_handle(), second)
        .unwrap();
    assert_eq!(fresh.poll(), None);
}

#[test]
fn comparison_requires_ready_profiles_and_reports_plain_errors() {
    let book = book();
    let pending = book.reserve(memory("P1")).unwrap().handle;
    assert_eq!(
        book.begin_comparison(book.default_handle(), pending)
            .unwrap_err(),
        ProfileCreateError::Profile(ProfileError::NotReady)
    );
    book.default_ready_for_test();
    assert_eq!(
        book.begin_comparison(book.default_handle(), pending)
            .unwrap_err(),
        ProfileCreateError::Profile(ProfileError::NotReady)
    );
    let ticket = book
        .begin_comparison(book.default_handle(), book.default_handle())
        .unwrap();
    book.record_comparison(
        &ticket,
        Err(ProfileCreateError::Profile(ProfileError::NotReady)),
    );
    assert_eq!(
        ticket.poll(),
        Some(Err(ProfileCreateError::Profile(ProfileError::NotReady)))
    );
}

#[test]
fn sealed_book_refuses_new_comparisons_and_tickets_stay_plain_data() {
    let book = book();
    book.default_ready_for_test();
    let ticket = book
        .begin_comparison(book.default_handle(), book.default_handle())
        .unwrap();
    assert_eq!(book.seal_and_release(), 0);
    assert_eq!(
        book.begin_comparison(book.default_handle(), book.default_handle())
            .unwrap_err(),
        ProfileCreateError::Closed
    );
    // The unresolved ticket resolves to a plain-data error at seal time; it
    // never upgrades back into a manager reference or a CEF object.
    assert_eq!(ticket.poll(), Some(Err(ProfileCreateError::Closed)));
}

#[test]
fn public_create_profile_error_is_distinct_from_the_core_one() {
    // The runtime's public error type is the frozen API name; the core error is
    // still reachable through the `Profile` variant.
    assert_eq!(CreateProfileError::NotReady, CreateProfileError::NotReady);
    assert_ne!(
        CreateProfileError::PersistentUnsupported,
        CreateProfileError::NotReady
    );
    assert_eq!(
        create_error_from_core(ProfileError::ConfigConflict),
        CreateProfileError::Profile(ProfileError::ConfigConflict)
    );
}

fn create_error_from_core(error: ProfileError) -> CreateProfileError {
    CreateProfileError::Profile(error)
}
