# Spec Delta

## ADDED Requirements

### Requirement: Host-initiated bounded strict-serial page find
The runtime host SHALL expose host-initiated page find (`Runtime::find`) and stop-finding (`Runtime::stop_finding`) operations bound to a live browser, reusing the bounded strict-serial find state machine, rejecting empty queries or retired bindings, and rejecting overlapping searches with backpressure while an active or quarantined find remains unresolved.

#### Scenario: Host initiates and steps through page find results
- **WHEN** the host calls `Runtime::find` on an active browser with a non-empty query string
- **THEN** the runtime dispatches the search on the CEF UI thread with a fresh `FindOperationId`, streams bounded match counts and active ordinals to the active find sink, and completes when CEF reports `final_update`

#### Scenario: Empty find query rejected
- **WHEN** the host calls `Runtime::find` with an empty search query
- **THEN** the runtime returns an invalid-request error without dispatching `find` to CEF or perturbing prior state

#### Scenario: Strict-serial backpressure and quarantine on ambiguous callback
- **WHEN** the host calls `Runtime::find` while an earlier find operation on the same browser has not completed `final_update` or after an uncorrelated positive CEF identifier quarantines that browser's find state
- **THEN** the runtime rejects the new search with `Backpressure`, preserves the unresolved or quarantined state until `final_update` or `OnBeforeClose`, and ignores stale callbacks at or below the identifier high-water mark

#### Scenario: Stop finding clears selection cleanly
- **WHEN** the host calls `Runtime::stop_finding` on a live browser with `clear_selection` set to true or false
- **THEN** the runtime dispatches CEF `stop_finding` on the UI thread and cancels any local in-flight host find tracking cleanly without affecting other browsers

### Requirement: Host-initiated bounded view source
The runtime host SHALL expose a host-initiated view-source operation (`Runtime::view_source`) bound to a live browser that validates main-frame HTML source against `SourceViewLimits` (rejecting over-budget payloads via `SourceFailure::Budget`) without requiring an open context menu, driving `SourceViewSink` (`started`, `publish`, `close`) while recording `SourceOutcome` in `SourceOperationStatus`.

#### Scenario: Host triggers view source without context menu
- **WHEN** the host calls `Runtime::view_source` on a live browser with an attached source-view sink
- **THEN** the runtime validates main-frame source text against `SourceViewLimits` (failing with `SourceFailure::Budget` if over budget), drives `SourceViewSink` (`started`, `publish(SourceViewRequest, control)`, and `close(id, control)`), and records `SourceOutcome` in `SourceOperationStatus`

#### Scenario: Unbound or closed browser rejected safely
- **WHEN** the host calls `Runtime::view_source` for a closed, unknown, or sink-less browser
- **THEN** the runtime returns a bounded error without spawning unmanaged CEF or OS windows

### Requirement: Direct per-browser find and view-source sink registration
For configurations where context menus are disabled or `NativeHwnd` child-HWND hosting is active without GPUI OSR context-menu services, the runtime host SHALL allow attaching and clearing direct per-browser `FindResultSink` and `SourceViewSink` instances while preserving all runtime bounds and core crate boundaries.

#### Scenario: Direct sinks receive updates across non-menu modes
- **WHEN** a host attaches direct per-browser find or source sinks and calls `Runtime::find` or `Runtime::view_source` with context menus disabled or in `NativeHwnd` mode
- **THEN** bounded find results reach the `FindResultSink` and view-source lifecycle calls (`started`, `publish(SourceViewRequest, control)`, `close(id, control)`) reach the `SourceViewSink` with `SourceOutcome` recorded in `SourceOperationStatus` under the same serialization, budget, and retirement guarantees
