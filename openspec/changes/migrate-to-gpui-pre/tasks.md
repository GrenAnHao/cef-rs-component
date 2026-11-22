# Tasks

## 1. Rollback point and baseline

- [x] 1.1 Record the current commit as the rollback point (`git rev-parse HEAD`, or copy the source tree if this checkout is not a repo) and verify `cargo build --locked --target x86_64-pc-windows-msvc` succeeds from it
- [x] 1.2 Record baseline numbers for comparison: build wall time, `target/bundle-debug` size, and frames delivered in an 8 s run, verified by a run that reproduces the previously measured ~473 frames

## 2. Landing A — core toolkit swap

- [x] 2.1 Change the dependency to `gpui = { version = "=0.3.12", package = "gpui-pre", default-features = false, features = ["windows-manifest"] }` and regenerate `Cargo.lock`; verify `cargo tree -i gpui` names exactly one core crate and `cargo tree -i gpui-pre` exists once
- [x] 2.2 Replace `gpui::ScrollWheelEvent` with `gpui::ScrollEvent` where the compiler reports deprecation; verify the build emits no deprecation warnings for it
- [x] 2.3 Align the `windows` crate in `Cargo.toml` to the version `gpui-pre` pulls, and fix `src/browser/ole_drag.rs` imports (`IDataObject_Impl`, `STGMEDIUM`, `STGMEDIUM_0`); verify `cargo check --target x86_64-pc-windows-msvc` reports zero errors
- [x] 2.4 Fix any remaining compiler errors in `src/windows.rs`, `src/ui.rs`, `src/browser/render_handler.rs`, `src/browser/d3d11_receiver.rs`; verify `cargo build --locked --target x86_64-pc-windows-msvc` succeeds
- [x] 2.5 Run the std-only suite (`rustc --edition 2024 --test src/lib.rs`) and verify the same test count passes as before the migration
- [x] 2.6 Run `scripts/check.ps1 -Interaction -Drag -Drop` and verify every self-test reports `SELF_TEST_RESULT=ok`
- [x] 2.7 Run `scripts/resize-test.py` and verify frames keep arriving while unfocused and the viewport follows in ≥6 distinct sizes
- [x] 2.8 Run `scripts/os-input-test.py` and verify `OS_INPUT_RESULT=ok`
- [x] 2.9 Run `scripts/drag-quality-test.py` and verify zero resampled blits
- [x] 2.10 Verify the accelerated path still matches software pixel-for-pixel by comparing `POC_FRAME_DUMP` output with `POC_ACCELERATED=1` and `=0`
- [x] 2.11 Update `docs/validation.md` with the migration record and the measured before/after numbers, and verify the documented commands run as written

## 3. Landing B — component library

- [ ] 3.1 Add `gpui-component = "=0.9.1"` and verify `cargo tree` shows one gpui core crate and that `gpui-component`'s `gpui-pre` requirement is satisfied by the same `0.3.12`
- [ ] 3.2 Replace the hand-written context menu in `src/ui.rs` with the component library's menu, preserving behaviour: items render disabled/enabled and checked as CEF reports them, a pick calls `Continue(id)`, dismissal calls `Cancel()`
- [ ] 3.3 Verify the context menu end to end with `POC_LOG_MENU=1`: the log shows the expected item labels for a click on the text field and on empty page area, and shows `CONTEXT_MENU pick <id>` after choosing an item
- [ ] 3.4 Replace the header/status bar in `src/ui.rs` with component-library equivalents, verifying the window still renders and reports frames
- [ ] 3.5 Re-run `scripts/check.ps1 -Interaction -Drag -Drop`, `scripts/resize-test.py`, `scripts/os-input-test.py` and `scripts/drag-quality-test.py`; verify all pass without weakening them
- [ ] 3.6 Record the post-migration build time and bundle size against the baseline from 1.2, and verify the difference is stated in `docs/validation.md`

## Workflow follow-up

- Archive the change with `openspec archive migrate-to-gpui-pre` once all tasks are checked.
- Verify the archived result with `openspec validate --all --strict`.
