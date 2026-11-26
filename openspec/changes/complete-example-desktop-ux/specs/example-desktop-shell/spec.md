# Spec Delta

## Purpose

Defines the interactive desktop browser shell behaviors for `gpui-cef-example`, including native file upload pickers, in-page Find, View Source, enhanced download controls, interactive Permission/Auth/Certificate/BeforeUnload modals, Win32 `NativeHwnd` airspace coordination, and clean desktop naming.

## ADDED Requirements

### Requirement: Native OS file picker integration for File Upload and Save
The example browser shell SHALL route `HostCapabilities::file_dialog` requests to GPUI native OS file pickers (`Open`, `OpenMultiple`, `OpenFolder`, and `Save`), resolving each `HostRequestId` at most once and clearing obsolete prompts if CEF cancels or the tab closes.

#### Scenario: File open, multi-file, and folder upload pickers
- **WHEN** web content invokes a file or directory upload prompt (`FileDialogKind::Open`, `OpenMultiple`, or `OpenFolder`)
- **THEN** the shell hides active `NativeHwnd` child views, opens `prompt_for_paths` with matching file/directory/multiple flags, restores child views afterward, and resolves the `HostRequestId` once with `Accept(paths)` bounded to `MAX_FILE_DIALOG_PATHS` or `Cancel`

#### Scenario: Save file dialog
- **WHEN** CEF triggers a `FileDialogKind::Save` request
- **THEN** the shell hides active `NativeHwnd` child views, opens `prompt_for_new_path` initialized with the suggested directory and filename, restores child views, and resolves the `HostRequestId` once with `Accept([path])` or `Cancel`

#### Scenario: Cancelled or stale file dialog ignored safely
- **WHEN** CEF cancels a pending file dialog request or the owning tab closes before the native OS dialog returns
- **THEN** the shell marks the request obsolete and discards any later async picker completion without double-resolving or panicking

### Requirement: In-page Find toolbar and keyboard navigation
The example browser shell SHALL provide a per-tab Find toolbar and keyboard/menu shortcuts (`Ctrl+F`, `F3`, `Shift+F3`, `Esc`, and `View -> Find in Page`) operating across both OSR and `NativeHwnd` modes under bounded strict-serial find execution.

#### Scenario: Open, search, step matches, and close Find bar
- **WHEN** the user presses `Ctrl+F` or selects `View -> Find in Page`
- **THEN** the active tab opens and focuses its Find toolbar; submitting text executes `Runtime::find`; pressing `F3` or `Enter` advances to the next match; pressing `Shift+F3` steps backward; the toolbar displays `active_ordinal / matches`; and pressing `Esc` or clicking Close hides the bar and calls `Runtime::stop_finding`

#### Scenario: Shortcut delivery when NativeHwnd holds focus
- **WHEN** a `NativeHwnd` child window holds Win32 keyboard focus
- **THEN** registered `ShortcutFilter` and `PreKeyFilter` rules intercept `Ctrl+F`, `F3`, `Shift+F3`, and `Esc` (while the Find bar is open) and trigger the shell Find actions without blocking ordinary page input when closed

#### Scenario: Strict-serial busy and quarantine feedback
- **WHEN** a Find operation is in flight or the browser's Find state enters quarantine
- **THEN** the Find toolbar displays an in-progress or quarantined status and refuses overlapping requests rather than attributing stale match updates to a newer query

### Requirement: Standalone View Source action and viewer
The example browser shell SHALL provide `Ctrl+U` and `View -> View Source` actions that invoke bounded host-initiated View Source in both OSR and `NativeHwnd` modes, presenting source text in a host-managed GPUI `"Page source"` window without unmanaged CEF popups.

#### Scenario: Open bounded Page source window
- **WHEN** the user presses `Ctrl+U` or selects `View -> View Source`
- **THEN** the shell invokes `Runtime::view_source` and opens a bounded host-managed `"Page source"` GPUI window (via `SourceViewServices` in OSR menu mode, or direct per-browser viewer otherwise) displaying valid snapshots bounded by `SourceViewLimits`

#### Scenario: Unavailable or over-budget source reports cleanly
- **WHEN** the target browser reports a View Source failure (including `SourceFailure::Budget`) or closed session
- **THEN** the shell shows an inline status notice and opens no unmanaged browser or OS window

### Requirement: Enhanced interactive Download lifecycle controls
The example browser shell SHALL enhance its `Download info` drawer with an optional `Ask Save As` prompt mode, per-download `Cancel` and `Open Containing Folder` controls, and deterministic cancellation of active or unconfirmed downloads on tab close.

#### Scenario: Optional Save-As destination prompt
- **WHEN** `Ask Save As` is enabled and CEF emits a `DownloadRequest`
- **THEN** the shell hides active `NativeHwnd` child views, prompts via `prompt_for_new_path`, validates the destination with `validate_download_destination`, and invokes `Runtime::confirm_download` or `Runtime::cancel_download` if dismissed or invalid

#### Scenario: Default download confirmation and action buttons
- **WHEN** a download is active or completed in `Download info`
- **THEN** the panel shows progress bytes and state, an interactive `Cancel` control calling `Runtime::cancel_download` while pending or in progress, and an `Open Containing Folder` control calling `reveal_path` once completed

#### Scenario: Deterministic download cancellation on tab close
- **WHEN** a browser tab closes while downloads belonging to that tab are unconfirmed or `InProgress`
- **THEN** the shell calls `Runtime::cancel_download` for each open download on that browser and clears any pending Save-As prompt for that tab

### Requirement: Interactive Permission prompt modal
The example browser shell SHALL handle `PermissionService` requests (`MediaAccess` and `PermissionPrompt`) through an interactive GPUI modal overlay displaying the requesting origin and permission types with explicit `Allow` and `Deny` controls instead of auto-denying.

#### Scenario: Allow or Deny site permission request
- **WHEN** web content triggers a `PermissionRequest`
- **THEN** the shell hides active `NativeHwnd` child views, presents the permission modal with origin and requested capability/media mask details, and resolves the `HostRequestId` once with `PermissionDecision::Allow { media_mask }` or `Deny`

#### Scenario: Permission request cancelled externally
- **WHEN** CEF cancels a pending permission request or its owning tab closes
- **THEN** the shell clears the pending modal entry and restores `NativeHwnd` visibility cleanly

### Requirement: Interactive HTTP Auth prompt modal with credential redaction
The example browser shell SHALL handle `AuthService` HTTP 401 and proxy challenges with an interactive GPUI modal containing username and visually masked password inputs, wrapping secrets in `RedactedSecret` and redacting all credentials from logs and HUDs.

#### Scenario: Submit masked credentials or cancel challenge
- **WHEN** CEF issues an `AuthRequest`
- **THEN** the shell hides active `NativeHwnd` child views, opens an authentication modal showing host, port, scheme, realm, and proxy indicator with bullet-masked password characters, resolves with `AuthDecision::Continue(AuthCredentials)` or `AuthDecision::Cancel`, and immediately wipes temporary input buffers

#### Scenario: No plaintext credential leakage
- **WHEN** an authentication challenge is displayed, submitted, cancelled, or logged
- **THEN** logs, debug snapshots, and HUD labels MUST NOT contain plaintext username or password bytes

### Requirement: Interactive TLS Certificate error and client certificate modals
The example browser shell SHALL handle `CertificateService` TLS server certificate errors and mTLS client certificate selections with interactive GPUI modals displaying the exposed request URL and error code or candidate certificate index, subject, and issuer before resolving single-use decisions.

#### Scenario: TLS server certificate warning prompt
- **WHEN** CEF reports a certificate error (`CertificateErrorRequest`)
- **THEN** the shell hides active `NativeHwnd` child views, presents `request_url` and `cert_error` with `Proceed` (`CertificateDecision::Accept { certificate_index: None }`) and `Cancel` actions, and resolves once

#### Scenario: Client certificate selection prompt
- **WHEN** a server requests a client certificate (`ClientCertificateRequest`) with candidate certificates
- **THEN** the shell displays candidate entries (`index`, `subject`, and `issuer`) and resolves with the selected `Accept { certificate_index: Some(index) }` or `Cancel`

### Requirement: Two-phase BeforeUnload confirmation for tab and window close
The example browser shell SHALL execute tab and window closure in two phases so `BeforeUnload` prompts appear as interactive `"Leave"` / `"Stay"` GPUI modals before destroying view callbacks, preserving the tab and window whenever the user chooses `"Stay"`.

#### Scenario: User chooses Stay to cancel close
- **WHEN** the user closes a tab or main window containing a page with a `beforeunload` handler and selects `"Stay"` in the modal
- **THEN** phase 1 leaves view bindings intact, the shell resolves `BeforeUnload` with `allow = false`, and upon `CloseCancelled` resets pending tab and window close flags so the tab and window remain open

#### Scenario: User chooses Leave to complete phase-2 teardown
- **WHEN** the user selects `"Leave"` (or the closing page has no `beforeunload` handler) and CEF reaches `SessionState::Closed`
- **THEN** phase 2 cancels remaining tab downloads, retires Find/Source/Menu/View state, removes the tab, and closes the main window if window shutdown was pending

#### Scenario: Reload BeforeUnload prompt
- **WHEN** CEF raises `BeforeUnloadRequested` with `is_reload = true`
- **THEN** the modal reflects reload confirmation wording and resolves `allow` according to user choice

### Requirement: NativeHwnd Win32 airspace coordination across overlays
Whenever any GPUI modal overlay or native OS file dialog is active in `NativeHwnd` mode, the example browser shell SHALL synchronously hide child browser HWNDs (`0x0`) and restore them once the overlay closes.

#### Scenario: Overlays never occluded by Win32 child HWND
- **WHEN** a File Picker, Permission, Auth, Certificate, BeforeUnload, JS dialog, notice, or context menu becomes active over a `NativeHwnd` tab
- **THEN** the shell sets child HWND visibility to hidden (`0x0` physical bounds) so GPUI controls remain visible and interactive, and restores visibility immediately after dismissal or cancellation

### Requirement: Clean desktop terminology, smoke preservation, and track isolation
The example browser shell SHALL contain zero WPF-specific wording in example UI copy, preserve existing `--smoke-ms <100..30000>` (`scripts/run-example.ps1 -SmokeSeconds <n>`) and JS bridge verification flows, and confine edits away from locked core crates, lockfiles, and parallel SDK release deliverables.

#### Scenario: Clean labels, passing smoke path, and isolated files
- **WHEN** `gpui-cef-example` is inspected and executed with `--smoke-ms <100..30000>` (or `scripts/run-example.ps1 -SmokeSeconds <n>`)
- **THEN** example UI text contains zero `wpf` or `WPF` strings, smoke verification exits successfully, and `crates/rust-cef-core`, all four `Cargo.lock` files, unrelated JSB RPC verification files, `README.md`, and `crates/rust-cef-gpui` SDK release targets remain untouched
