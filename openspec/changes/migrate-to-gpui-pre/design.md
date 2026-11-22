# Design

## Context

See proposal.md — Why. The relevant measured facts, from a throwaway copy of this repository:

- `gpui-component 0.9.1` declares `gpui = { version = "=0.3.12", package = "gpui-pre" }`.
- Adding both `gpui 0.2.2` and `gpui-component 0.9.1` to one project resolves **both** `gpui 0.2.2` and `gpui-pre 0.3.12` plus `gpui-base 0.9.1`, about 750 packages, and `cargo tree -i gpui` confirms they are separate crates.
- Switching our dependency to `gpui-pre 0.3.12` and running `cargo check --target x86_64-pc-windows-msvc` produced **zero errors in our code except one**, and that one is unrelated to gpui: `ole_drag.rs` imports `windows::Win32::System::Com::{IDataObject_Impl, STGMEDIUM, STGMEDIUM_0}`, which moved between the `windows 0.61` we pin and the `0.58` that `gpui-pre` pulls in.
- The only gpui-side fallout is deprecation warnings: `gpui::ScrollWheelEvent` is renamed to `ScrollEvent` (4 occurrences).
- `gpui-pre 0.3.12` still exposes `paint_image`, `handle_input`, `EntityInputHandler`, `RenderImage::new`, `Application::new`, `scale_factor`, `rgb`; the API surface this project uses is intact.

## Goals / Non-Goals

**Goals:**

- Land the toolkit migration as its own verifiable step, with every existing self-test green before any component library is added.
- Keep the migration reversible: the pre-migration commit must build and pass the same suites.
- Then replace the hand-written context menu and header controls with `gpui-component` equivalents.

**Non-Goals:**

- Zero-copy compositing. That needs a Windows surface primitive in gpui itself and is unaffected by this migration; the shared-texture read-back stays.
- Dragging out of the page (link/image to Explorer). Still declined for the documented `DoDragDrop` reason.
- Rewriting the frame pipeline, input translation, or the CEF glue. Only what the compiler forces.

## Decisions

**Migrate wholesale rather than shim two gpui versions.** Keeping `0.2.2` alongside the component library would mean two incompatible `Entity`/`Window`/`RenderImage` types; a component's `Button` could never be placed in a 0.2.2 view tree. Shimming would also double build time and binary size. Rejected.

**Migrate in two landings: core first, components second.** The measured error surface is small, but it mixes two kinds of breakage — toolkit renames and `windows` crate bindings. Landing them together would make a red test ambiguous. The first landing changes only `Cargo.toml`, `ScrollWheelEvent` → `ScrollEvent`, and the `windows` version alignment in `ole_drag.rs`.

**Align our `windows` crate to the version `gpui-pre` already pulls (0.58).** Two `windows` versions in one binary is legal but doubles the metadata and risks two incompatible COM interfaces at link time. Since `ole_drag` is the only consumer, following gpui's choice is cheaper than fighting it. Alternative — keep 0.61 and accept both — rejected on link-time ambiguity risk for `IDataObject`.

**Verify with the suites already in the repo.** `scripts/check.ps1` (`-Interaction`, `-Drag`, `-Drop`), `scripts/resize-test.py`, `scripts/os-input-test.py`, `scripts/drag-quality-test.py`, plus the std-only unit tests. No new harness; the point is to prove the old behaviour survives.

**Pin the exact version and commit the lockfile.** `gpui-pre` is a pre-release line whose API moves; a caret requirement would let a patch bump change behaviour silently. `gpui-component` pins `=0.3.12` too, so both move together or not at all.

## Risks / Trade-offs

- [Toolkit is a pre-release; API can move again] → Pin `=0.3.12` and keep the migration to compiler-forced changes, so the next bump is equally mechanical.
- [The component library pins the same exact version] → Treat `gpui-pre` and `gpui-component` as one versioned unit; upgrade both in a single change.
- [A behaviour regression that compiles fine] → The parity requirement is checked by running every existing suite; a green build alone is explicitly not acceptance.
- [Larger dependency graph, slower builds] → Accepted; it is the cost of the component library. Record build time before and after.
- [The `windows 0.58` downgrade breaks the D3D11 receiver] → It is already exercised by `POC_ACCELERATED=1`; verify with the pixel-fingerprint comparison, not just compilation.
- [Unknown API changes in code paths we do not compile on Windows] → The std-only test module and `validation/api-check` provide a second compiler view.

## Migration Plan

1. Create a worktree or tag the pre-migration commit as the rollback point.
2. Landing A — core toolkit: change the dependency, fix `ScrollEvent`, align `windows`, rebuild, run every suite.
3. Landing B — components: add `gpui-component`, replace the context menu and header, rebuild, run every suite.
4. Rollback is `git revert` of the landing; nothing touches user data or the CEF runtime layout.
