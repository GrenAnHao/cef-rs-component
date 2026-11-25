use super::*;

fn recovery_service() -> ServiceState {
    ServiceState {
        thread: 0,
        active: Cell::new(true),
        current: Cell::new(None),
        dispatching: Cell::new(false),
        attempt_key: Cell::new(None),
        attempt_route: Cell::new(None),
        aborted: Cell::new(false),
        next_nonce: Cell::new(1),
        diagnostics: RefCell::new(NativeKeyboardDiagnostics::default()),
        policy: RefCell::new(None),
        routes: RefCell::new(Vec::new()),
    }
}

fn in_flight(route_id: u64) -> Envelope {
    Envelope {
        hwnd: ptr::null_mut(),
        route_id,
        nonce: 1,
        key: packet(false, 1),
        claimed: true,
        host_consumed: false,
        text_allowed: true,
        epoch: Some(1),
    }
}

fn packet(up: bool, lparam: isize) -> NativeKey {
    NativeKey {
        vk: 0x41,
        lparam,
        system: false,
        up,
        expected_epoch: None,
        modifiers: MouseModifiers {
            shift: true,
            ..MouseModifiers::default()
        },
    }
}

#[test]
fn nested_up_before_down_ledger_revokes_original_route_and_nonce() {
    let state = recovery_service();
    let consumed = Cell::new(false);
    let cleaned = RefCell::new(Vec::new());
    state.run_dispatch(
        0x41,
        || {
            state.attempt_route.set(Some(1));
            state.current.set(Some(in_flight(1)));
            state.recover_reentrant(
                0x41,
                std::iter::empty(),
                || consumed.set(true),
                |id, reason| {
                    assert!(consumed.get());
                    assert!(state.aborted.get() && state.current.get().is_none());
                    cleaned.borrow_mut().push((id, reason));
                    true
                },
            );
            // This is the production outer-dispatch gate before ledger write
            // and TranslateMessage, not an emulated GPUI or Win32 message pump.
            assert!(state.aborted.get());
            assert!(state.current.get().is_none());
        },
        |_, _| panic!("unexpected outer panic cleanup"),
    );
    assert!(consumed.get());
    assert_eq!(*cleaned.borrow(), [(1, NativeKeyboardFailure::Reentrant)]);
    assert!(!state.dispatching.get());
}

#[test]
fn nested_b_release_cleans_b_and_suspended_a() {
    let state = recovery_service();
    let consumed = Cell::new(false);
    let cleaned = RefCell::new(Vec::new());
    state.run_dispatch(
        0x41,
        || {
            state.attempt_route.set(Some(1));
            state.current.set(Some(in_flight(1)));
            // The production route ledger supplies B (2) as the UP origin,
            // even though its VK differs from A's suspended DOWN.
            state.recover_reentrant(
                0x42,
                [2],
                || consumed.set(true),
                |id, reason| {
                    assert!(consumed.get());
                    cleaned.borrow_mut().push((id, reason));
                    true
                },
            );
            assert!(state.aborted.get() && state.current.get().is_none());
        },
        |_, _| panic!("unexpected outer panic cleanup"),
    );
    assert_eq!(
        *cleaned.borrow(),
        [
            (2, NativeKeyboardFailure::Reentrant),
            (1, NativeKeyboardFailure::Reentrant),
        ]
    );
}

#[test]
fn admitted_down_panic_cleans_attempt_route_before_guard_drops_it() {
    let state = recovery_service();
    let cleaned = RefCell::new(Vec::new());
    state.run_dispatch(
        0x41,
        || {
            state.attempt_route.set(Some(7));
            state.current.set(Some(in_flight(7)));
            panic!("host or GPUI callback failed after DOWN admission");
        },
        |id, reason| {
            assert!(state.dispatching.get());
            assert_eq!(state.attempt_route.get(), Some(7));
            assert!(state.aborted.get() && state.current.get().is_none());
            cleaned.borrow_mut().push((id, reason));
            true
        },
    );
    assert_eq!(*cleaned.borrow(), [(7, NativeKeyboardFailure::Panic)]);
    assert_eq!(state.diagnostics.borrow().panics, 1);
    assert!(!state.dispatching.get());
    assert_eq!(state.attempt_route.get(), None);
}

#[test]
fn win64_message_layout_and_remove_only() {
    assert_eq!(std::mem::size_of::<Msg>(), 48);
    assert!(selected_keyboard(0, PM_REMOVE, WM_KEYDOWN));
    assert!(selected_keyboard(0, PM_REMOVE, WM_SYSKEYUP));
    assert!(!selected_keyboard(-1, PM_REMOVE, WM_KEYDOWN));
    assert!(!selected_keyboard(0, 0, WM_KEYDOWN));
    assert!(!selected_keyboard(0, PM_REMOVE, WM_QUIT));
    assert!(!selected_keyboard(0, PM_REMOVE, WM_NULL));
}

#[test]
fn raw_key_preserves_virtual_code_scan_repeat_and_system() {
    let key = packet(false, (0x1e << 16) | (1 << 30) | 1);
    let InputKind::Key {
        windows_key_code,
        native_key_code,
        up,
        repeat,
        system,
        modifiers,
    } = key.kind()
    else {
        panic!("expected key");
    };
    assert_eq!(windows_key_code, 0x41);
    assert_eq!(native_key_code, key.lparam as i32);
    assert!(!up && repeat && !system && modifiers.shift);
    let InputKind::Key {
        up, repeat, system, ..
    } = NativeKey {
        system: true,
        up: true,
        ..key
    }
    .kind()
    else {
        panic!("expected keyup");
    };
    assert!(up && !repeat && system);
}

#[test]
fn private_nonce_is_directional_single_claim_not_a_matching_vk() {
    let key = packet(true, (0x1e << 16) | (1 << 31) | 1);
    let envelope = Envelope {
        hwnd: ptr::null_mut(),
        route_id: 1,
        nonce: 29,
        key,
        claimed: false,
        host_consumed: false,
        text_allowed: false,
        epoch: None,
    };
    let slot = Cell::new(Some(envelope));
    let stroke = keystroke(key, 29);
    assert_eq!(stroke.key, native_name(29));
    assert!(!matches_envelope(envelope, &stroke, false));
    let foreign = keystroke(key, 30);
    assert!(claim_nonce(&slot, &foreign, true, false, false, None).is_none());
    assert!(claim_nonce(&slot, &stroke, false, false, false, None).is_none());
    assert_eq!(
        claim_nonce(&slot, &stroke, true, false, false, None).map(|native| native.vk),
        Some(0x41)
    );
    assert!(claim_nonce(&slot, &stroke, true, false, false, None).is_none());
    assert!(slot.get().unwrap().claimed);
}

#[test]
fn ime_process_key_sentinel_resolves_to_the_real_virtual_key() {
    // Raw evidence from a live IME-enabled window: the DOWN carried 0xE5 while
    // its UP carried 0x4E. Both must index the same ledger entry.
    assert_eq!(message_virtual_key(0x4e, || 0), 0x4e);
    assert_eq!(message_virtual_key(VK_PROCESSKEY, || 0x4e), 0x4e);
    // An unresolvable or out-of-range IMM answer is never a fabricated key.
    assert_eq!(message_virtual_key(VK_PROCESSKEY, || 0), VK_PROCESSKEY);
    assert_eq!(message_virtual_key(VK_PROCESSKEY, || 0x1ff), VK_PROCESSKEY);
}
