# Spec Delta

## Purpose

Constrain which gpui version the project builds against, and which observable behaviours must survive a change of that version. It exists because the GUI toolkit is a hard-version dependency: a component library compiled against a different gpui produces incompatible types, and a migration that silently drops input or rendering behaviour is worse than not migrating.

## ADDED Requirements

### Requirement: Single GUI toolkit version

The build MUST resolve exactly one gpui-family core crate. A second, differently-versioned gpui in the same dependency graph is a build failure, not a warning.

#### Scenario: Duplicate gpui is rejected

- **WHEN** the dependency graph resolves both `gpui` and `gpui-pre` (or two versions of either)
- **THEN** the change is rejected before release, and the graph is reduced to one core crate

#### Scenario: Component library agrees with the core version

- **WHEN** a UI component library is added
- **THEN** the core gpui version it depends on is the same version the application builds against

### Requirement: Pinned patch version

The gpui dependency MUST pin an exact patch version, and the lockfile MUST be committed.

#### Scenario: Unpinned version is rejected

- **WHEN** the dependency reads `gpui = "0.3"` or a caret requirement that could resolve to a different patch
- **THEN** the requirement is changed to an exact `=x.y.z` pin before merging

### Requirement: Behaviour parity after toolkit migration

Migrating the toolkit MUST NOT remove any behaviour the project already shipped: off-screen rendering, mouse/keyboard/IME forwarding, window-size and DPI synchronisation, shared-texture frames, multiple browser instances, and drag-and-drop in both directions.

#### Scenario: Rendering still works

- **WHEN** the application runs after a toolkit migration
- **THEN** it reports a first CEF frame, sustains the target frame rate, and paints frames at their own pixel size without resampling

#### Scenario: Input still reaches the page

- **WHEN** the scripted input self-test drives mouse, wheel, keyboard and IME events
- **THEN** the page reports each event class, as it did before the migration

#### Scenario: Size and DPI still track the window

- **WHEN** the window is resized in steps while it does not have focus
- **THEN** CEF repaints at each intermediate size, and frames continue arriving while unfocused

### Requirement: Migration is verified by the existing suites

A toolkit migration MUST be judged by the suites already in the repository, not by a compile succeeding.

#### Scenario: Existing checks pass unchanged

- **WHEN** the migration is declared complete
- **THEN** the pure-logic unit tests, the scripted input/drag/drop self-tests, the resize/focus test and the OS-level input test all pass on the new toolkit without weakening their assertions
