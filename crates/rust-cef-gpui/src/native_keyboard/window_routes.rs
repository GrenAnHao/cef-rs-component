//! HWND groups resolve actual focus. There is no cached owner or message queue.
use super::*;

impl WindowRoutes {
    pub(super) fn process(&self, message: *mut Msg, original: Msg, key: NativeKey) {
        if !self.service.active.get() {
            return;
        }
        if self.service.dispatching.get() {
            let routes = self.routes();
            let origins = routes
                .iter()
                .filter(|route| {
                    route.owned_keys.get()[key.vk as usize]
                        || route.host_exceptions.get()[key.vk as usize]
                })
                .map(|route| route.id);
            self.service.recover_reentrant(
                key.vk,
                origins,
                || neutralize(message),
                |id, reason| {
                    if let Some(route) = routes.iter().find(|route| route.id == id) {
                        route.fail(reason);
                        true
                    } else {
                        false
                    }
                },
            );
            return;
        }
        // Recovery runs BEFORE the dispatch guard discards the exact in-flight
        // route, including a DOWN not yet reflected in the pressed-key ledger.
        self.service.run_dispatch(
            key.vk,
            || self.process_owned(message, original, key),
            |id, reason| self.fail_route(id, reason),
        );
    }

    fn routes(&self) -> Vec<Rc<Route>> {
        self.service
            .routes
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .collect()
    }

    fn fail_route(&self, id: u64, reason: NativeKeyboardFailure) -> bool {
        if let Some(route) = self.routes().into_iter().find(|route| route.id == id) {
            route.fail(reason);
            true
        } else {
            false
        }
    }

    fn process_owned(&self, message: *mut Msg, original: Msg, key: NativeKey) {
        let index = key.vk as usize;
        let all = self.routes();
        let origin = all
            .iter()
            .find(|route| route.owned_keys.get()[index] || route.host_exceptions.get()[index])
            .cloned();
        self.service
            .attempt_route
            .set(origin.as_ref().map(|route| route.id));
        let local: Vec<_> = all
            .iter()
            .filter(|route| route.hwnd == self.hwnd)
            .cloned()
            .collect();
        let mut app = self.app.clone();
        let focused = app.update_window(self.window_handle, |_, window, _| {
            if !window.is_window_active()
                || unsafe { GetFocus() } != self.hwnd
                || unsafe { GetForegroundWindow() } != self.hwnd
            {
                return None;
            }
            local
                .iter()
                .find(|route| {
                    route.token.matches_window(route.window_id)
                        && route.view.upgrade().is_some()
                        && route.focus.is_focused(window)
                })
                .cloned()
        });
        if self.service.aborted.get() {
            return;
        }
        let owner = match focused {
            Ok(owner) => owner,
            Err(_) => {
                if let Some(origin) = origin {
                    neutralize(message);
                    origin.fail(NativeKeyboardFailure::BorrowedApp);
                } else {
                    self.service
                        .failure(NativeKeyboardFailure::BorrowedApp, None);
                }
                return;
            }
        };
        self.service
            .attempt_route
            .set(owner.as_ref().or(origin.as_ref()).map(|route| route.id));
        // Ownership is fixed for an entire physical gesture, including repeats.
        // A host action may move focus; its UP still cannot leak into Chromium.
        if origin
            .as_ref()
            .is_some_and(|route| route.host_exceptions.get()[index])
        {
            let origin = origin.unwrap();
            self.service.attempt_route.set(Some(origin.id));
            neutralize(message);
            let mut keys = origin.host_exceptions.get();
            keys[index] = !key.up;
            origin.host_exceptions.set(keys);
            let policy = self.service.policy.borrow().clone();
            let handled = app.update_window(self.window_handle, |_, window, cx| {
                if let Some(callback) = policy {
                    callback(key.vk, key.modifiers, key.up, window, cx);
                }
            });
            if self.service.aborted.get() {
                return;
            }
            if handled.is_err() {
                origin.fail(NativeKeyboardFailure::WindowUnavailable);
            } else {
                let mut diagnostics = self.service.diagnostics.borrow_mut();
                diagnostics.host_shortcuts = diagnostics.host_shortcuts.saturating_add(1);
            }
            if key.up && backend_modifier(key.vk) {
                self.sync_modifier_up(&original, key);
            }
            return;
        }
        if let Some(origin) = origin.as_ref() {
            if origin.owned_keys.get()[index]
                && origin.browser.input_snapshot().is_ok_and(|snapshot| {
                    snapshot.epoch != origin.owned_epochs.get()[index] || snapshot.cleanup_pending
                })
            {
                origin.forget_key(index);
                if key.up || key.lparam as u32 & (1 << 30) != 0 {
                    neutralize(message);
                    if key.up && backend_modifier(key.vk) {
                        self.sync_modifier_up(&original, key);
                    }
                    return;
                }
            }
            if (origin.owned_keys.get()[index] || origin.host_exceptions.get()[index])
                && owner.as_ref().is_none_or(|owner| owner.id != origin.id)
            {
                neutralize(message);
                origin.cancel(InputResetReason::FocusLost);
                if key.up && backend_modifier(key.vk) {
                    self.sync_modifier_up(&original, key);
                }
                return;
            }
        }
        let Some(owner) = owner else {
            // Host controls are not browser input or a Browser failure. Their
            // exact original MSG is left to the existing platform dispatcher.
            return;
        };
        if key.up && origin.is_none() {
            neutralize(message);
            let mut diagnostics = self.service.diagnostics.borrow_mut();
            diagnostics.orphan_up = diagnostics.orphan_up.saturating_add(1);
            drop(diagnostics);
            if backend_modifier(key.vk) {
                self.sync_modifier_up(&original, key);
            }
            return;
        }
        self.service.attempt_route.set(Some(owner.id));
        let mut key = key;
        if key.up {
            key.expected_epoch = Some(owner.owned_epochs.get()[index]);
        }
        owner.process(message, original, key);
        if self.service.aborted.get() {
            return;
        }
        if key.up && backend_modifier(key.vk) && unsafe { (*message).message } == WM_NULL {
            self.sync_modifier_up(&original, key);
        }
    }

    fn sync_modifier_up(&self, original: &Msg, key: NativeKey) {
        debug_assert!(key.up && backend_modifier(key.vk));
        if unsafe { GetPropW(original.hwnd, PROPERTY.as_ptr()) } != self as *const Self as Handle {
            self.service
                .failure(NativeKeyboardFailure::WindowUnavailable, None);
            return;
        }
        // Exactly the ACTUAL removed UP, never fabricated. The fixed SDK's
        // modifier branch yields ModifiersChanged or None, not another KeyUp.
        unsafe {
            DispatchMessageW(original);
        }
        if self.service.aborted.get() {
            return;
        }
        // Publish state even when the SDK deduped its private baseline. An
        // identical state notification is not another raw key or text command.
        let modifiers = gpui_modifiers(current_modifiers());
        let capslock = gpui::Capslock {
            on: unsafe { GetKeyState(0x14) & 1 != 0 },
        };
        let mut app = self.app.clone();
        if app
            .update_window(self.window_handle, |_, window, cx| {
                window.dispatch_event(
                    PlatformInput::ModifiersChanged(gpui::ModifiersChangedEvent {
                        modifiers,
                        capslock,
                    }),
                    cx,
                );
            })
            .is_err()
        {
            self.service
                .failure(NativeKeyboardFailure::WindowUnavailable, None);
        }
    }
}

fn backend_modifier(vk: u32) -> bool {
    // Exact branch in fixed GPUI Windows handle_key_event. Normal-key UPs must
    // NOT be dispatched again: that would produce another raw GPUI KeyUp.
    matches!(
        vk,
        VK_SHIFT | VK_CONTROL | VK_MENU | 0xa4 | 0xa5 | VK_LWIN | VK_RWIN | 0x14
    )
}
