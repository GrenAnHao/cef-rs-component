//! Public-only 10.1 consumer. This host, not the library, owns Application/exit.
#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
fn main() {
    eprintln!("component-host supports Windows x64 only");
    std::process::exit(2)
}
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn main() {
    std::process::exit(windows::run())
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod windows {
    mod dispatch;
    mod distribution_restart;
    #[cfg(feature = "validation")]
    mod exhaustion;
    #[cfg(feature = "validation")]
    mod exhaustion_evidence;
    #[cfg(feature = "validation")]
    mod find_view;
    mod input;
    mod input_probe;
    #[cfg(feature = "validation")]
    mod menu_default;
    #[cfg(feature = "validation")]
    mod paint_latency;
    mod pointer_probe;
    mod popup;
    #[cfg(feature = "validation")]
    mod print_panel_probe;
    mod resize;
    #[cfg(feature = "validation")]
    mod source_viewer;
    mod unfocused;
    #[cfg(feature = "validation")]
    mod visibility;
    #[cfg(feature = "validation")]
    mod visibility_scene;
    use gpui::{
        App, AppContext, AsyncApp, Bounds, Context, Entity, IntoElement, Render, Window,
        WindowBounds, WindowHandle, WindowOptions, div, prelude::*, px, rgb, size,
    };
    use rust_cef_gpui::{
        CefBrowser, Runtime, RuntimeServices, ViewDiagnostics,
        contracts::{
            BrowserConfig, BrowserId, ProfileStorage, RenderPreference, RuntimeConfig,
            RuntimeFiles, RuntimeState, ThreadMode, TrustedContentPolicy, Viewport,
        },
        dispatcher::{self, UiWakeReceiver, UiWakeSender},
        ports::{
            BrowserEvent, BrowserEventEnvelope, BrowserEventSink, CloseMode, EngineError,
            FrameSink, OwnedFrame, PublishResult, SurfaceKind, UiDispatcher,
        },
        runtime::{RuntimeDiagnostics, ShutdownStatus, StartError},
    };
    use std::{
        cell::Cell,
        future::Future,
        path::{Path, PathBuf},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        task::Poll,
        time::{Duration, Instant},
    };
    const TITLE: &str = "GPUI-CEF public host";
    const URL: &str = "data:text/html,%3Cbody%20style%3D%22margin%3A0%3Bbackground%3Argb(26%2C76%2C126)%22%3E%3C%2Fbody%3E";
    thread_local! {static APP_BORROW:Cell<bool>=const{Cell::new(false)};}
    struct Borrow;
    impl Borrow {
        fn enter() -> Self {
            APP_BORROW.with(|b| assert!(!b.replace(true), "nested host GPUI borrow"));
            Self
        }
    }
    impl Drop for Borrow {
        fn drop(&mut self) {
            APP_BORROW.with(|b| b.set(false));
        }
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum HostScenario {
        Embed,
        Dispatch,
        ProfileDispatch,
        PaintLatency,
        Popup,
        Input,
        ImeObserve,
        Resize,
        Unfocused,
        Exhaustion,
        Visibility,
        SourceViewer,
        FindView,
        MenuDefault,
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum PrintCleanupFault {
        ReloadAfterDetach,
        CloseAfterDetach,
        BeforePresent,
    }

    #[derive(Clone)]
    struct Options {
        mode: ThreadMode,
        backend: RenderPreference,
        report: PathBuf,
        helper: PathBuf,
        gate: PathBuf,
        scenario: HostScenario,
        interactive_input_approved: bool,
        passive_distribution: bool,
        print_confirmation_probe: bool,
        print_navigation_probe: bool,
        #[cfg(feature = "validation")]
        print_cleanup_fault_probe: Option<PrintCleanupFault>,
        restart: Option<distribution_restart::Restart>,
        timing_profile_peer: bool,
        #[cfg(feature = "validation")]
        timing_profile_windows: bool,
        #[cfg(feature = "validation")]
        timing_measure_both: bool,
        timing_window_pressure: bool,
        #[cfg(feature = "validation")]
        timing_window_print_pressure: bool,
        #[cfg(feature = "validation")]
        timing_window_menu_pressure: bool,
        #[cfg(feature = "validation")]
        timing_window_menu_flush_pressure: bool,
        #[cfg(feature = "validation")]
        timing_extra_flush_fault: bool,
        #[cfg(feature = "validation")]
        timing_window_source_pressure: bool,
        #[cfg(feature = "validation")]
        timing_window_find_pressure: bool,
        #[cfg(feature = "validation")]
        timing_window_event_pressure: bool,
        manual_foreground: bool,
        initial_foreground_approved: bool,
        exhaustion_manual_foreground: bool,
        visibility_operations_approved: bool,
        visibility_manual_foreground: bool,
        #[cfg(feature = "validation")]
        resize_capture_evidence: bool,
        resize_manual_foreground: bool,
        resize_passive: bool,
        resize_passive_right: bool,
        #[cfg(feature = "validation")]
        resize_exhaustion: bool,
        #[cfg(feature = "validation")]
        resize_verified_transitions: bool,
        menu_services_probe: bool,
        menu_foreground_approved: bool,
        menu_dismiss_probe: bool,
        menu_devtools_probe: bool,
        menu_find_probe: bool,
        menu_source_probe: bool,
        menu_edit_probe: bool,
        menu_spelling_probe: bool,
        menu_structure_probe: bool,
        menu_submenu_probe: bool,
    }
    fn initial_foreground_flag(
        value: Option<&str>,
        scenario: HostScenario,
    ) -> Result<bool, String> {
        match value {
            None | Some("false") => Ok(false),
            Some("true")
                if cfg!(feature = "validation") && scenario == HostScenario::Exhaustion =>
            {
                Ok(true)
            }
            Some("true") => Err("initial foreground approval is validation Exhaustion-only".into()),
            _ => Err("invalid initial foreground approval".into()),
        }
    }
    fn exhaustion_manual_foreground_flag(
        value: Option<&str>,
        scenario: HostScenario,
        ordinary: bool,
    ) -> Result<bool, String> {
        match value {
            None | Some("false") => Ok(false),
            Some("true") if cfg!(feature = "validation") && scenario == HostScenario::Exhaustion && !ordinary => Ok(true),
            Some("true") => Err("manual Exhaustion foreground requires validation Exhaustion without ordinary foreground approval".into()),
            _ => Err("invalid manual Exhaustion foreground flag".into()),
        }
    }
    fn visibility_manual_foreground_flag(
        value: Option<&str>,
        scenario: HostScenario,
        operations_approved: bool,
    ) -> Result<bool, String> {
        match value {
            None | Some("false") => Ok(false),
            Some("true")
                if cfg!(feature = "validation")
                    && scenario == HostScenario::Visibility
                    && operations_approved =>
            {
                Ok(true)
            }
            Some("true") => {
                Err("manual visibility foreground requires approved validation Visibility".into())
            }
            _ => Err("invalid manual visibility foreground flag".into()),
        }
    }
    fn visibility_operations_flag(
        value: Option<&str>,
        scenario: HostScenario,
    ) -> Result<bool, String> {
        match (value, scenario) {
            (Some("true"), HostScenario::Visibility) if cfg!(feature = "validation") => Ok(true),
            (None | Some("false"), HostScenario::Visibility) => {
                Err("Visibility requires explicit window operation approval".into())
            }
            (None | Some("false"), _) => Ok(false),
            _ => Err(
                "visibility operation approval is non-default validation Visibility-only".into(),
            ),
        }
    }
    fn parse() -> Result<Options, String> {
        let mut args = std::env::args().skip(1);
        let mut fields = std::collections::BTreeMap::new();
        while let Some(key) = args.next() {
            let value = args.next().ok_or("missing argument value")?;
            if fields.insert(key, value).is_some() {
                return Err("duplicate argument".into());
            }
        }
        let mode = match fields.remove("--mode").as_deref() {
            Some("Integrated") => ThreadMode::Integrated,
            Some("Threaded") => ThreadMode::Threaded,
            _ => return Err("invalid mode".into()),
        };
        let backend = match fields.remove("--backend").as_deref() {
            Some("Software") => RenderPreference::SoftwareRequired,
            Some("D3d11") => RenderPreference::D3d11Required,
            _ => return Err("invalid backend".into()),
        };
        let report = PathBuf::from(fields.remove("--report").ok_or("missing report")?);
        let helper = PathBuf::from(fields.remove("--helper").ok_or("missing helper")?);
        let gate = PathBuf::from(
            fields
                .remove("--observer-gate")
                .ok_or("missing observer gate")?,
        );
        let scenario = match fields.remove("--scenario").as_deref() {
            None | Some("Embed") => HostScenario::Embed,
            Some("Dispatch") => HostScenario::Dispatch,
            Some("ProfileDispatch") => HostScenario::ProfileDispatch,
            Some("PaintLatency") if cfg!(feature = "validation") => HostScenario::PaintLatency,
            Some("PaintLatency") => return Err("PaintLatency requires validation feature".into()),
            Some("Popup") => HostScenario::Popup,
            Some("Input") => HostScenario::Input,
            Some("ImeObserve") => HostScenario::ImeObserve,
            Some("Resize") => HostScenario::Resize,
            Some("Unfocused") => HostScenario::Unfocused,
            Some("Exhaustion") if cfg!(feature = "validation") => HostScenario::Exhaustion,
            Some("Exhaustion") => {
                return Err("Exhaustion requires non-default validation feature".into());
            }
            Some("FindView") if cfg!(feature = "validation") => HostScenario::FindView,
            Some("FindView") => {
                return Err("FindView requires non-default validation feature".into());
            }
            // The owner approved running this same real-click slice on both modes
            // and both required backends, so the cell is no longer pinned. It is
            // still validation-only and still requires desktop input approval.
            Some("MenuDefault") if cfg!(feature = "validation") => HostScenario::MenuDefault,
            Some("MenuDefault") => {
                return Err("MenuDefault requires non-default validation feature".into());
            }
            Some("SourceViewer") if cfg!(feature = "validation") => HostScenario::SourceViewer,
            Some("SourceViewer") => {
                return Err("SourceViewer requires non-default validation feature".into());
            }
            Some("Visibility") if cfg!(feature = "validation") => HostScenario::Visibility,
            Some("Visibility") => {
                return Err("Visibility requires non-default validation feature".into());
            }
            _ => return Err("invalid public host scenario".into()),
        };
        let interactive_input_approved =
            match fields.remove("--interactive-input-approved").as_deref() {
                None | Some("false") => false,
                Some("true") => true,
                _ => return Err("invalid interactive input approval".into()),
            };
        if matches!(
            scenario,
            HostScenario::SourceViewer | HostScenario::FindView
        ) && interactive_input_approved
        {
            return Err("SourceViewer and FindView do not accept desktop input approval".into());
        }
        if matches!(
            scenario,
            HostScenario::ProfileDispatch | HostScenario::PaintLatency
        ) && interactive_input_approved
        {
            return Err("passive validation does not accept interactive input".into());
        }
        let manual_foreground = match fields.remove("--manual-foreground").as_deref() {
            None | Some("false") => false,
            Some("true") => true,
            _ => return Err("invalid manual foreground flag".into()),
        };
        let initial_foreground_approved = initial_foreground_flag(
            fields.remove("--initial-foreground-approved").as_deref(),
            scenario,
        )?;
        let exhaustion_manual_foreground = exhaustion_manual_foreground_flag(
            fields.remove("--exhaustion-manual-foreground").as_deref(),
            scenario,
            initial_foreground_approved,
        )?;
        let visibility_operations_approved = visibility_operations_flag(
            fields.remove("--visibility-operations-approved").as_deref(),
            scenario,
        )?;
        let visibility_manual_foreground = visibility_manual_foreground_flag(
            fields.remove("--visibility-manual-foreground").as_deref(),
            scenario,
            visibility_operations_approved,
        )?;
        if manual_foreground && !matches!(scenario, HostScenario::Input) {
            return Err("manual foreground setup is Input-only".into());
        }
        let resize_passive = match fields.remove("--resize-passive").as_deref() {
            None | Some("false") => false,
            Some("true")
                if cfg!(feature = "validation")
                    && scenario == HostScenario::Resize
                    && !interactive_input_approved =>
            {
                true
            }
            _ => {
                return Err(
                    "passive Resize requires a validation Resize without interactive approval"
                        .into(),
                );
            }
        };
        if matches!(
            scenario,
            HostScenario::Popup
                | HostScenario::Input
                | HostScenario::ImeObserve
                | HostScenario::Resize
                | HostScenario::Unfocused
                | HostScenario::Exhaustion
                | HostScenario::MenuDefault
        ) && !interactive_input_approved
            && !resize_passive
        {
            return Err(
                "Window/input validation requires --interactive-input-approved true".into(),
            );
        }
        let _resize_capture_evidence = match fields.remove("--resize-capture-evidence").as_deref() {
            None | Some("false") => false,
            Some("true")
                if cfg!(feature = "validation")
                    && scenario == HostScenario::Resize
                    && backend == RenderPreference::D3d11Required
                    && (interactive_input_approved || resize_passive) =>
            {
                true
            }
            _ => {
                return Err(
                    "resize capture evidence requires approved validation Resize with D3d11".into(),
                );
            }
        };
        #[cfg(feature = "validation")]
        let resize_exhaustion = match fields.remove("--resize-exhaustion").as_deref() {
            None | Some("false") => false,
            Some("true") if scenario == HostScenario::Resize && resize_passive => true,
            Some("true") => return Err("resize exhaustion requires passive Resize".into()),
            _ => return Err("invalid resize exhaustion flag".into()),
        };
        let resize_passive_right = match fields.remove("--resize-passive-right").as_deref() {
            None | Some("false") => false,
            Some("true") if resize_passive => true,
            _ => return Err("right-side Resize placement requires passive Resize".into()),
        };
        let _resize_verified_transitions =
            match fields.remove("--resize-verified-transitions").as_deref() {
                None | Some("false") => false,
                Some("true") if resize_passive && _resize_capture_evidence => true,
                _ => {
                    return Err(
                        "verified Resize transitions require passive D3d11 capture evidence".into(),
                    );
                }
            };
        let resize_manual_foreground = match fields.remove("--resize-manual-foreground").as_deref()
        {
            None | Some("false") => false,
            Some("true") if _resize_capture_evidence && !resize_passive => true,
            _ => {
                return Err(
                    "manual Resize foreground requires approved Resize capture evidence".into(),
                );
            }
        };
        let menu_services_probe = match fields.remove("--menu-services-probe").as_deref() {
            None | Some("false") => false,
            Some("true")
                if cfg!(feature = "validation")
                    && scenario == HostScenario::Embed
                    && mode == ThreadMode::Threaded
                    && backend == RenderPreference::SoftwareRequired =>
            {
                true
            }
            _ => {
                return Err("MenuServicesProbe requires validation Embed Threaded Software".into());
            }
        };
        let menu_foreground_approved = match fields.remove("--menu-foreground-approved").as_deref()
        {
            None | Some("false") => false,
            // MenuDefault may take this ordinary foreground approval on any single
            // mode/backend cell. SourceViewer and FindView may also request it,
            // because their readiness gate needs the owner window active while
            // their own drivers never activate and still send no desktop input.
            Some("true")
                if cfg!(feature = "validation")
                    && ((scenario == HostScenario::MenuDefault && interactive_input_approved)
                        || matches!(
                            scenario,
                            HostScenario::SourceViewer | HostScenario::FindView
                        )) =>
            {
                true
            }
            _ => return Err(
                "menu foreground approval is validation MenuDefault Threaded Software input-only or SourceViewer/FindView"
                    .into(),
            ),
        };
        let menu_submenu_probe = match fields.remove("--menu-submenu-probe").as_deref() {
            Some("true") => true,
            Some("false") => false,
            Some(other) => {
                return Err(format!(
                    "--menu-submenu-probe expects true|false, got {other}"
                ));
            }
            None => false,
        };
        let menu_structure_probe = match fields.remove("--menu-structure-probe").as_deref() {
            Some("true") => true,
            Some("false") => false,
            Some(other) => {
                return Err(format!(
                    "--menu-structure-probe expects true|false, got {other}"
                ));
            }
            None => false,
        };
        let menu_spelling_probe = match fields.remove("--menu-spelling-probe").as_deref() {
            None | Some("false") => false,
            Some("true")
                if cfg!(feature = "validation") && scenario == HostScenario::MenuDefault =>
            {
                true
            }
            _ => return Err("MenuSpellingProbe requires validation MenuDefault".into()),
        };
        let menu_edit_probe = match fields.remove("--menu-edit-probe").as_deref() {
            None | Some("false") => false,
            Some("true")
                if cfg!(feature = "validation") && scenario == HostScenario::MenuDefault =>
            {
                true
            }
            _ => return Err("MenuEditProbe requires validation MenuDefault".into()),
        };
        let menu_source_probe = match fields.remove("--menu-source-probe").as_deref() {
            None | Some("false") => false,
            Some("true")
                if cfg!(feature = "validation") && scenario == HostScenario::MenuDefault =>
            {
                true
            }
            _ => return Err("MenuSourceProbe requires validation MenuDefault".into()),
        };
        let menu_find_probe = match fields.remove("--menu-find-probe").as_deref() {
            None | Some("false") => false,
            Some("true")
                if cfg!(feature = "validation") && scenario == HostScenario::MenuDefault =>
            {
                true
            }
            _ => return Err("MenuFindProbe requires validation MenuDefault".into()),
        };
        let menu_devtools_probe = match fields.remove("--menu-devtools-probe").as_deref() {
            None | Some("false") => false,
            // Real click on the opt-in "Show DevTools (Inspect)" row. Validation
            // only, same menu slice, needs desktop input approval.
            Some("true")
                if cfg!(feature = "validation")
                    && scenario == HostScenario::MenuDefault
                    && interactive_input_approved =>
            {
                true
            }
            _ => return Err("MenuDevtoolsProbe requires validation MenuDefault input".into()),
        };
        let menu_dismiss_probe = match fields.remove("--menu-dismiss-probe").as_deref() {
            None | Some("false") => false,
            Some("true")
                if cfg!(feature = "validation")
                    && scenario == HostScenario::MenuDefault
                    && mode == ThreadMode::Threaded
                    && backend == RenderPreference::SoftwareRequired
                    && interactive_input_approved =>
            {
                true
            }
            _ => {
                return Err(
                    "MenuDismissProbe requires validation MenuDefault Threaded Software input"
                        .into(),
                );
            }
        };
        let passive_distribution = match fields.remove("--passive-distribution").as_deref() {
            None => false,
            Some("true")
                if scenario == HostScenario::Embed
                    && !interactive_input_approved
                    && !menu_services_probe =>
            {
                true
            }
            _ => {
                return Err(
                    "passive distribution requires plain Embed without input or menu probes".into(),
                );
            }
        };
        let timing_profile_peer = match fields.remove("--timing-profile-peer").as_deref() {
            None => false,
            Some("true") if scenario == HostScenario::PaintLatency => true,
            _ => return Err("timing-profile-peer requires passive PaintLatency".into()),
        };
        let _timing_profile_windows = match fields.remove("--timing-profile-windows").as_deref() {
            None => false,
            Some("true") if timing_profile_peer && cfg!(feature = "validation") => true,
            _ => return Err("timing-profile-windows requires a passive PaintLatency peer".into()),
        };
        let _timing_window_pressure = match fields.remove("--timing-window-pressure").as_deref() {
            None => false,
            Some("true") if _timing_profile_windows => true,
            _ => return Err("timing-window-pressure requires two passive timing windows".into()),
        };
        let _timing_measure_both = match fields.remove("--timing-measure-both").as_deref() {
            None => false,
            Some("true") if _timing_profile_windows && !_timing_window_pressure => true,
            _ => {
                return Err(
                    "timing-measure-both requires passive two-window timing without pressure"
                        .into(),
                );
            }
        };
        let _timing_window_menu_pressure =
            match fields.remove("--timing-window-menu-pressure").as_deref() {
                None => false,
                Some("true") if _timing_window_pressure => true,
                _ => {
                    return Err(
                        "timing-window-menu-pressure requires passive two-window pressure".into(),
                    );
                }
            };
        let _timing_window_menu_flush_pressure = match fields
            .remove("--timing-window-menu-flush-pressure")
            .as_deref()
        {
            None => false,
            Some("true") if _timing_window_menu_pressure => true,
            _ => {
                return Err(
                    "timing-window-menu-flush-pressure requires passive menu window pressure"
                        .into(),
                );
            }
        };
        let _timing_extra_flush_fault =
            match fields.remove("--timing-extra-flush-fault").as_deref() {
                None => false,
                Some("true")
                    if _timing_window_menu_flush_pressure
                        && mode == ThreadMode::Integrated
                        && backend == RenderPreference::SoftwareRequired =>
                {
                    true
                }
                _ => return Err(
                    "timing-extra-flush-fault requires passive Integrated Software flush pressure"
                        .into(),
                ),
            };
        let _timing_window_source_pressure =
            match fields.remove("--timing-window-source-pressure").as_deref() {
                None => false,
                Some("true")
                    if _timing_window_menu_flush_pressure && !_timing_extra_flush_fault =>
                {
                    true
                }
                _ => return Err(
                    "timing-window-source-pressure requires passive flush pressure without fault"
                        .into(),
                ),
            };
        let _timing_window_find_pressure = match fields
            .remove("--timing-window-find-pressure")
            .as_deref()
        {
            None => false,
            Some("true") if _timing_window_source_pressure => true,
            _ => return Err("timing-window-find-pressure requires passive Source pressure".into()),
        };
        let _timing_window_event_pressure = match fields
            .remove("--timing-window-event-pressure")
            .as_deref()
        {
            None => false,
            Some("true") if _timing_window_find_pressure => true,
            _ => return Err("timing-window-event-pressure requires passive Find pressure".into()),
        };
        let _timing_window_print_pressure =
            match fields.remove("--timing-window-print-pressure").as_deref() {
                None => false,
                Some("true") if _timing_window_event_pressure && !_timing_extra_flush_fault => true,
                _ => {
                    return Err(
                        "timing-window-print-pressure requires passive mixed-event pressure".into(),
                    );
                }
            };
        let restart = distribution_restart::Restart::parse(&mut fields)?;
        let print_confirmation_probe = match fields.remove("--print-confirmation-probe").as_deref() {
            None => false,
            Some("true")
                if cfg!(feature = "validation") && passive_distribution && restart.is_none()
                    && backend == RenderPreference::SoftwareRequired => true,
            _ => return Err("print confirmation probe requires validation, passive Embed, Software and no restart".into()),
        };
        let print_navigation_probe = match fields.remove("--print-navigation-probe").as_deref() {
            None => false,
            Some("true") if print_confirmation_probe => true,
            _ => return Err("print navigation probe requires passive print confirmation".into()),
        };
        let _print_cleanup_fault_probe = match fields
            .remove("--print-cleanup-fault-probe")
            .as_deref()
        {
            None => None,
            Some("reload-after-detach") if print_navigation_probe => {
                Some(PrintCleanupFault::ReloadAfterDetach)
            }
            Some("close-after-detach") if print_confirmation_probe && !print_navigation_probe => {
                Some(PrintCleanupFault::CloseAfterDetach)
            }
            Some("before-present") if print_confirmation_probe && !print_navigation_probe => {
                Some(PrintCleanupFault::BeforePresent)
            }
            _ => {
                return Err(
                    "print cleanup fault is not eligible for this passive print validation".into(),
                );
            }
        };
        if restart.is_some() && !passive_distribution {
            return Err("profile restart requires passive distribution Embed".into());
        }
        if !fields.is_empty()
            || ![&report, &helper, &gate]
                .into_iter()
                .all(|p| p.is_absolute())
        {
            return Err("unknown flag or nonabsolute path".into());
        }
        Ok(Options {
            mode,
            backend,
            report,
            helper,
            gate,
            scenario,
            interactive_input_approved,
            passive_distribution,
            print_confirmation_probe,
            print_navigation_probe,
            #[cfg(feature = "validation")]
            print_cleanup_fault_probe: _print_cleanup_fault_probe,
            restart,
            timing_profile_peer,
            #[cfg(feature = "validation")]
            timing_profile_windows: _timing_profile_windows,
            #[cfg(feature = "validation")]
            timing_measure_both: _timing_measure_both,
            timing_window_pressure: _timing_window_pressure,
            #[cfg(feature = "validation")]
            timing_window_print_pressure: _timing_window_print_pressure,
            #[cfg(feature = "validation")]
            timing_window_menu_pressure: _timing_window_menu_pressure,
            #[cfg(feature = "validation")]
            timing_window_menu_flush_pressure: _timing_window_menu_flush_pressure,
            #[cfg(feature = "validation")]
            timing_extra_flush_fault: _timing_extra_flush_fault,
            #[cfg(feature = "validation")]
            timing_window_source_pressure: _timing_window_source_pressure,
            #[cfg(feature = "validation")]
            timing_window_find_pressure: _timing_window_find_pressure,
            #[cfg(feature = "validation")]
            timing_window_event_pressure: _timing_window_event_pressure,
            manual_foreground,
            initial_foreground_approved,
            exhaustion_manual_foreground,
            visibility_operations_approved,
            visibility_manual_foreground,
            #[cfg(feature = "validation")]
            resize_capture_evidence: _resize_capture_evidence,
            resize_manual_foreground,
            resize_passive,
            resize_passive_right,
            #[cfg(feature = "validation")]
            resize_exhaustion,
            #[cfg(feature = "validation")]
            resize_verified_transitions: _resize_verified_transitions,
            menu_services_probe,
            menu_foreground_approved,
            menu_dismiss_probe,
            menu_devtools_probe,
            menu_find_probe,
            menu_source_probe,
            menu_edit_probe,
            menu_spelling_probe,
            menu_structure_probe,
            menu_submenu_probe,
        })
    }
    fn quote(s: &str) -> String {
        let mut out = String::from("\"");
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }
    fn object(fields: Vec<(&str, String)>) -> String {
        format!(
            "{{{}}}",
            fields
                .into_iter()
                .map(|(k, v)| format!("{}:{v}", quote(k)))
                .collect::<Vec<_>>()
                .join(",")
        )
    }
    fn optional_number(n: Option<u64>) -> String {
        n.map_or_else(|| "null".into(), |v| v.to_string())
    }
    fn bool_json(value: bool) -> String {
        value.to_string()
    }
    fn write_json(path: &Path, text: String) -> Result<(), String> {
        std::fs::write(path, text).map_err(|e| e.to_string())
    }
    #[derive(Default)]
    struct Stats {
        created: AtomicU64,
        closed: AtomicU64,
        valid_frames: AtomicU64,
        ffi: AtomicU64,
        borrow_violations: AtomicU64,
        error: Mutex<Option<String>>,
        #[cfg(feature = "validation")]
        fault: Mutex<exhaustion::ExpectedFault>,
        #[cfg(feature = "validation")]
        capture_events: Mutex<exhaustion_evidence::CaptureEvents>,
        #[cfg(feature = "validation")]
        exhaustion_letterbox: Mutex<Option<BrowserId>>,
        #[cfg(feature = "validation")]
        resize_letterbox: Mutex<Option<BrowserId>>,
        #[cfg(feature = "validation")]
        print_load: Mutex<Option<print_panel_probe::LoadWitness>>,
        #[cfg(feature = "validation")]
        window_pressure: paint_latency::window_pressure::EventLane,
    }
    impl Stats {
        fn ffi(&self) {
            self.ffi.fetch_add(1, Ordering::Relaxed);
            if APP_BORROW.with(Cell::get) {
                self.borrow_violations.fetch_add(1, Ordering::Relaxed);
            }
        }
        fn fail(&self, e: impl Into<String>) {
            self.error.lock().unwrap().get_or_insert(e.into());
        }
        fn check(&self) -> Result<(), String> {
            self.error.lock().unwrap().clone().map_or(Ok(()), Err)
        }
    }
    struct Events(Arc<Stats>);
    impl BrowserEventSink for Events {
        fn publish(&self, e: BrowserEventEnvelope) -> PublishResult {
            self.0.ffi();
            #[cfg(feature = "validation")]
            if let Some(result) = self.0.window_pressure.observe(&e) {
                return result;
            }
            #[cfg(feature = "validation")]
            if let Some(load) = self.0.print_load.lock().unwrap().as_mut() {
                load.observe(&e);
            }
            match e.event {
                BrowserEvent::Created => {
                    self.0.created.fetch_add(1, Ordering::Relaxed);
                }
                BrowserEvent::Closed => {
                    self.0.closed.fetch_add(1, Ordering::Relaxed);
                }
                BrowserEvent::CaptureFailed(error) => {
                    // Owned, bounded diagnostic copy only; original gate below is unchanged.
                    #[cfg(feature = "validation")]
                    self.0
                        .capture_events
                        .lock()
                        .unwrap()
                        .record(e.browser, e.sequence, &error);
                    // This original event stays in the loss-bounded ledger.
                    // Only the armed validation Browser can defer the decision
                    // to the next GPUI turn and the final retirement check.
                    #[cfg(feature = "validation")]
                    if exhaustion_evidence::is_letterbox_candidate(&error)
                        && (*self.0.exhaustion_letterbox.lock().unwrap() == Some(e.browser)
                            || *self.0.resize_letterbox.lock().unwrap() == Some(e.browser))
                    {
                        return PublishResult::Published;
                    }
                    // Validation-only, complete identity/phase/one-event ledger.
                    // Ordinary scenes never arm it; all other errors still fail.
                    #[cfg(feature = "validation")]
                    if self
                        .0
                        .fault
                        .lock()
                        .unwrap()
                        .observe(e.browser, e.sequence, &error)
                    {
                        return PublishResult::Published;
                    }
                    self.0.fail(format!("native event: {error:?}"));
                }
                BrowserEvent::CreateFailed(error) => {
                    self.0.fail(format!("native event: {error:?}"))
                }
                BrowserEvent::RendererTerminated { .. } => self.0.fail("renderer terminated"),
                _ => {}
            }
            PublishResult::Published
        }
    }
    struct Frames(Arc<Stats>);
    impl FrameSink for Frames {
        fn publish(&self, f: OwnedFrame) -> PublishResult {
            self.0.ffi();
            if f.info.surface == SurfaceKind::View {
                let p = ((f.info.height as usize / 2) * f.info.width as usize
                    + f.info.width as usize / 2)
                    * 4;
                if f.pixels.get(p..p + 4) == Some(&[126, 76, 26, 255]) {
                    self.0.valid_frames.fetch_add(1, Ordering::Relaxed);
                }
            }
            PublishResult::Published
        }
    }
    struct Wake {
        sender: UiWakeSender,
        stats: Arc<Stats>,
    }
    impl UiDispatcher for Wake {
        fn wake(&self) -> Result<(), EngineError> {
            self.stats.ffi();
            self.sender.wake()
        }
    }
    struct HostRoot {
        browser: Option<Entity<CefBrowser>>,
        secondary: Option<Entity<CefBrowser>>,
        multi: bool,
        secondary_detached: bool,
        extent: (f32, f32),
        renders: u64,
        detached: bool,
        // Host-owned fault presentation, not a default component overlay.
        fault_label: Option<&'static str>,
        menu: Option<std::rc::Rc<std::cell::RefCell<rust_cef_gpui::DefaultMenuPresenter>>>,
        print_confirmation:
            Option<Entity<rust_cef_gpui::print_confirmation::PrintConfirmation>>,
    }
    impl Render for HostRoot {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.renders += 1;
            let menu = self
                .menu
                .as_ref()
                .and_then(|menu| menu.borrow().element(window, cx));
            let footer = self.fault_label.unwrap_or(if self.detached {
                "Host remains alive after CefBrowser release"
            } else {
                "Public CefBrowser embedded below host chrome"
            });
            let content = div()
                .w(px(self.extent.0))
                .h(px(self.extent.1))
                .overflow_hidden()
                .bg(if self.detached {
                    rgb(0x00aa44)
                } else {
                    rgb(0x151515)
                });
            let content = if let Some(browser) = self.browser.as_ref() {
                content.child(browser.clone())
            } else {
                content
            };
            let content = if self.multi {
                let secondary = div()
                    .w(px(320.))
                    .h(px(240.))
                    .flex_shrink_0()
                    .overflow_hidden()
                    .bg(if self.secondary_detached {
                        rgb(0x00aa44)
                    } else {
                        rgb(0x151515)
                    });
                let secondary = if let Some(view) = self.secondary.as_ref() {
                    secondary.child(view.clone())
                } else {
                    secondary
                };
                div()
                    .flex()
                    .flex_row()
                    .gap(px(20.))
                    .child(content.flex_shrink_0())
                    .child(secondary)
            } else {
                div().flex().flex_row().child(content.flex_shrink_0())
            };
            let confirmation = self.print_confirmation.clone().map(|panel| {
                div()
                    .absolute()
                    .left(px(16.))
                    .top(px(60.))
                    .w(px(280.))
                    .child(panel)
            });
            div()
                .relative()
                .size_full()
                .flex()
                .flex_col()
                .bg(rgb(0x202020))
                .child(
                    div()
                        .h(px(48.))
                        .w_full()
                        .bg(rgb(0x9f2222))
                        .text_color(rgb(0xffffff))
                        .child("Existing host Application / window"),
                )
                .child(content)
                .child(div().text_color(rgb(0xffffff)).child(footer))
                .children(confirmation)
                .children(menu)
        }
    }
    enum Tick {
        Wake,
        Timer,
    }
    async fn wait(
        cx: &mut AsyncApp,
        receiver: &UiWakeReceiver,
        duration: Duration,
    ) -> Result<Tick, String> {
        let timer = cx.background_executor().timer(duration);
        let mut timer = std::pin::pin!(timer);
        let mut wake = std::pin::pin!(receiver.recv());
        std::future::poll_fn(|ctx| {
            if let Poll::Ready(ok) = wake.as_mut().poll(ctx) {
                return Poll::Ready(if ok {
                    Ok(Tick::Wake)
                } else {
                    Err("dispatcher closed".into())
                });
            }
            if timer.as_mut().poll(ctx).is_ready() {
                return Poll::Ready(Ok(Tick::Timer));
            }
            Poll::Pending
        })
        .await
    }
    async fn turn(
        cx: &mut AsyncApp,
        rt: &mut Runtime,
        receiver: &UiWakeReceiver,
        require_wake: bool,
    ) -> Result<bool, String> {
        turn_observed(cx, rt, receiver, require_wake, None).await
    }
    async fn turn_observed(
        cx: &mut AsyncApp,
        rt: &mut Runtime,
        receiver: &UiWakeReceiver,
        require_wake: bool,
        pump_markers: Option<&[std::rc::Rc<Cell<bool>>]>,
    ) -> Result<bool, String> {
        let mode = rt.mode();
        let timeout = if mode == ThreadMode::Integrated {
            Duration::from_millis(4)
        } else if require_wake {
            Duration::from_secs(10)
        } else {
            Duration::from_millis(50)
        };
        let tick = wait(cx, receiver, timeout).await?;
        if mode == ThreadMode::Threaded && require_wake && matches!(tick, Tick::Timer) {
            return Err("Threaded source delivery requires a real wake, not timer polling".into());
        }
        if mode == ThreadMode::Integrated {
            if APP_BORROW.with(Cell::get) {
                return Err("pump inside GPUI borrow".into());
            }
            let _observation = input_probe::PumpObservation::enter(pump_markers)?;
            rt.pump().map_err(|e| format!("pump: {e:?}"))?;
        }
        Ok(matches!(tick, Tick::Wake))
    }
    fn update<R>(
        cx: &mut AsyncApp,
        root: WindowHandle<HostRoot>,
        f: impl FnOnce(&mut HostRoot, &mut Window, &mut Context<HostRoot>) -> R,
    ) -> Result<R, String> {
        let _borrow = Borrow::enter();
        root.update(cx, f).map_err(|e| e.to_string())
    }
    fn app_update<R>(cx: &mut AsyncApp, update: impl FnOnce(&mut App) -> R) -> Result<R, String> {
        let _borrow = Borrow::enter();
        Ok(cx.update(update))
    }
    async fn rendered(cx: &mut AsyncApp, root: WindowHandle<HostRoot>) -> Result<(), String> {
        let (sender, receiver) = dispatcher::channel();
        update(cx, root, move |_, window, _| {
            window.on_next_frame(move |_, _| {
                let _ = sender.wake();
            })
        })?;
        if matches!(
            wait(cx, &receiver, Duration::from_secs(5)).await?,
            Tick::Timer
        ) {
            return Err("GPUI frame completion timeout".into());
        }
        Ok(())
    }
    #[cfg(feature = "validation")]
    fn menu_fixture_readiness_timeout(
        cx: &mut AsyncApp,
        root: WindowHandle<HostRoot>,
        scenario: &str,
        menu_ready: bool,
    ) -> Result<String, String> {
        let active = update(cx, root, |_, window, _| window.is_window_active())?;
        Ok(format!(
            "{scenario}: original View readiness timed out (window_active={active}, menu_ready={menu_ready}); the host does not request foreground activation"
        ))
    }

    fn metrics_json(d: &ViewDiagnostics) -> String {
        object(vec![
            ("schema", "1".into()),
            ("paint_calls", d.paint_calls.to_string()),
            ("painted_sequence", d.painted_sequence.to_string()),
            ("painted_generation", d.painted_generation.to_string()),
            ("frame_width", d.frame_width.to_string()),
            ("frame_height", d.frame_height.to_string()),
            // Debug keeps the fractional marker: f32 1.0 must stay 1.0 in JSON so
            // the float-typed receipt predicates cannot see an integer at 100%.
            ("scale_factor", format!("{:?}", d.scale_factor)),
            (
                "surface_bounds",
                format!(
                    "[{},{},{},{}]",
                    d.surface_bounds.0, d.surface_bounds.1, d.surface_bounds.2, d.surface_bounds.3
                ),
            ),
            ("created_images", d.created_images.to_string()),
            ("released_images", d.released_images.to_string()),
            ("consumed_updates", d.consumed_updates.to_string()),
            ("binding_retired", d.binding_retired.to_string()),
            ("binding_revision", d.binding_revision.to_string()),
            ("resync_attempts", d.resync_attempts.to_string()),
            ("rejected_paints", d.rejected_paints.to_string()),
            ("pending_resync", d.pending_resync.to_string()),
            ("clicks_submitted", d.clicks_submitted.to_string()),
            ("clicks_rejected", d.clicks_rejected.to_string()),
            ("pending_click", d.pending_click.to_string()),
            (
                "popup",
                object(vec![
                    ("generation", d.popup.generation.to_string()),
                    ("visible", d.popup.visible.to_string()),
                    (
                        "bounds",
                        format!(
                            "[{},{},{},{}]",
                            d.popup.bounds.0, d.popup.bounds.1, d.popup.bounds.2, d.popup.bounds.3
                        ),
                    ),
                    ("frame_width", d.popup.frame_width.to_string()),
                    ("frame_height", d.popup.frame_height.to_string()),
                    ("created_images", d.popup.created_images.to_string()),
                    ("released_images", d.popup.released_images.to_string()),
                    ("paint_calls", d.popup.paint_calls.to_string()),
                    ("painted_sequence", d.popup.painted_sequence.to_string()),
                    ("painted_generation", d.popup.painted_generation.to_string()),
                    (
                        "painted_bounds",
                        format!(
                            "[{},{},{},{}]",
                            d.popup.painted_bounds.0,
                            d.popup.painted_bounds.1,
                            d.popup.painted_bounds.2,
                            d.popup.painted_bounds.3
                        ),
                    ),
                ]),
            ),
            (
                "last_error",
                d.last_error
                    .as_ref()
                    .map_or_else(|| "null".into(), |e| quote(e)),
            ),
        ])
    }
    #[derive(Default)]
    struct Outcome {
        restart_detail: Option<String>,
        menu_services_verified: bool,
        menu_default_detail: Option<String>,
        source_viewer_verified: bool,
        source_viewer_detail: Option<String>,
        find_view_verified: bool,
        find_view_detail: Option<String>,
        error: Option<String>,
        finished: bool,
        diag: Option<RuntimeDiagnostics>,
        paint: Option<ViewDiagnostics>,
        wake_consumed: u64,
        detached_alive: bool,
        #[cfg(feature = "validation")]
        resize_exhaustion_verified: bool,
        view_release_observed_closing: bool,
        pressure_closed_before_view_release: bool,
        duplicate_refused: bool,
        browser: Option<BrowserId>,
        additional_browsers: Vec<BrowserId>,
        additional_roots: Vec<WindowHandle<HostRoot>>,
        dispatch_detail: Option<String>,
        paint_timing_detail: Option<String>,
        paint_timing_rows: Vec<String>,
        paint_timing_peer_rows: Vec<String>,
        paint_timing_retired: Option<String>,
        paint_timing_peer: Option<String>,
        paint_timing_peer_retired: Option<String>,
        #[cfg(feature = "validation")]
        window_flush: Option<paint_latency::window_flush::Held>,
        #[cfg(feature = "validation")]
        window_print: Option<paint_latency::window_print::Held>,
        window_flush_detail: Option<String>,
        window_flush_cleanup_detail: Option<String>,
        #[cfg(feature = "validation")]
        window_source_enabled: bool,
        #[cfg(feature = "validation")]
        window_source: Option<paint_latency::window_source::Held>,
        window_source_detail: Option<String>,
        popup_detail: Option<String>,
        input_detail: Option<String>,
        resize_detail: Option<String>,
        unfocused_detail: Option<String>,
        exhaustion_detail: Option<String>,
        visibility_detail: Option<String>,
        print_confirmation_detail: Option<String>,
        #[cfg(feature = "validation")]
        failure_evidence_attempted: bool,
        #[cfg(feature = "validation")]
        initial_foreground: exhaustion::InitialForeground,
        released_view: Option<ViewDiagnostics>,
        budget_used: Option<usize>,
        fatal_exit: Option<i32>,
    }
    async fn body(
        cx: &mut AsyncApp,
        rt: &mut Runtime,
        receiver: &UiWakeReceiver,
        stats: &Stats,
        root: WindowHandle<HostRoot>,
        options: &Options,
        out: &mut Outcome,
    ) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(25);
        while rt.state() != RuntimeState::Ready {
            if Instant::now() >= deadline {
                return Err("Runtime readiness timeout".into());
            }
            turn(cx, rt, receiver, false).await?;
            stats.check()?;
        }
        let (initial_url, profile) = if let Some(restart) = &options.restart {
            let profile = restart.ready_profile(cx, rt, receiver).await?;
            (restart.page.clone(), Some(profile))
        } else {
            (URL.into(), None)
        };
        #[cfg(feature = "validation")]
        if options.print_navigation_probe {
            // Threaded CEF can deliver the initial main load before create_browser returns.
            *stats.print_load.lock().unwrap() = Some(print_panel_probe::LoadWitness::new());
        }
        let handle = rt
            .create_browser(BrowserConfig {
                initial_url,
                profile,
                render_preference: options.backend,
                viewport: Viewport {
                    width: 320,
                    height: 240,
                    scale: 1.0,
                },
                options: Default::default(),
            })
            .map_err(|e| format!("create: {e:?}"))?;
        let browser = handle.snapshot().browser;
        out.browser = Some(browser);
        #[cfg(feature = "validation")]
        if options.print_navigation_probe {
            stats
                .print_load
                .lock()
                .unwrap()
                .as_mut()
                .ok_or("original print load observer missing")?
                .bind(browser)?;
        }
        let attachment = rt
            .attach_browser(&handle)
            .map_err(|e| format!("attachment: {e:?}"))?;
        out.duplicate_refused = rt.attach_browser(&handle).is_err();
        if !out.duplicate_refused {
            return Err("duplicate View attachment accepted".into());
        }
        let entity = update(cx, root, move |host, window, cx| {
            let entity = CefBrowser::new(attachment, window, cx)?;
            host.browser = Some(entity.clone());
            cx.notify();
            Ok::<_, rust_cef_gpui::ViewError>(entity)
        })?
        .map_err(|e| format!("view construction: {e:?}"))?;
        let mut menu_probe = None;
        #[cfg(feature = "validation")]
        let mut print_case = None;
        let result = async {
            loop {
                if Instant::now() >= deadline {
                    return Err("owned source delivery timeout".into());
                }
                let woke = turn(cx, rt, receiver, true).await?;
                stats.check()?;
                if woke {
                    out.wake_consumed += 1;
                }
                if !woke {
                    continue;
                }
                let consumed = update(cx, root, |_, window, cx| {
                    entity.update(cx, |view, cx| view.consume(window, cx))
                })?
                .map_err(|e| format!("consume: {e:?}"))?;
                let d = update(cx, root, |_, _, cx| entity.read(cx).diagnostics())?;
                if consumed
                    && d.created_images > 0
                    && stats.valid_frames.load(Ordering::Acquire) > 0
                {
                    break;
                }
            }
            rendered(cx, root).await?;
            let d = update(cx, root, |_, _, cx| entity.read(cx).diagnostics())?;
            if d.paint_calls == 0
                || d.painted_sequence == 0
                || d.frame_width == 0
                || d.frame_height == 0
                || d.last_error.is_some()
            {
                return Err(format!("actual CefBrowser paint missing: {d:?}"));
            }
            write_json(
                &options.report.with_file_name("view-metrics.json"),
                metrics_json(&d),
            )?;
            out.paint = Some(d);
            #[cfg(feature = "validation")]
            if options.print_confirmation_probe {
                let baseline = if options.print_navigation_probe {
                    Some(
                        print_panel_probe::await_initial_load(cx, rt, receiver, stats, &handle)
                            .await?,
                    )
                } else {
                    None
                };
                print_case = Some(print_panel_probe::PanelCase::arm(
                    cx, root, rt, &handle, &entity,
                )?);
                if options.print_cleanup_fault_probe == Some(PrintCleanupFault::BeforePresent) {
                    return Err(
                        "test-induced failure before original print panel installation".into(),
                    );
                }
                let case = print_case.as_mut().expect("armed original panel");
                case.present(cx, root, rt, &entity, options).await?;
                if let Some(baseline) = baseline {
                    case.reload(
                        cx,
                        root,
                        rt,
                        receiver,
                        stats,
                        &handle,
                        baseline,
                        options.print_cleanup_fault_probe
                            == Some(PrintCleanupFault::ReloadAfterDetach),
                    )
                    .await?;
                }
            }
            if options.menu_services_probe {
                use rust_cef_gpui::menu::{
                    ContextMenuServices, MenuContextLimits, MenuLimits, MenuServiceError,
                };
                let nz = |n| std::num::NonZeroUsize::new(n).unwrap();
                let mut services = ContextMenuServices::new(
                    nz(1),
                    nz(1),
                    MenuLimits {
                        max_entries: nz(32),
                        max_label_bytes: nz(256),
                        max_total_label_bytes: nz(2048),
                    },
                    MenuContextLimits {
                        max_frame_id_bytes: nz(512),
                        max_text_bytes: nz(4096),
                        max_suggestions: nz(5),
                        max_total_bytes: nz(8192),
                    },
                );
                let (target, window_id, duplicate) = update(cx, root, |_, window, cx| {
                    let target = services.register(&entity, window, cx)?;
                    let duplicate = matches!(
                        services.register(&entity, window, cx),
                        Err(MenuServiceError::AlreadyRegistered)
                    );
                    Ok::<_, MenuServiceError>((
                        target,
                        window.window_handle().window_id().as_u64(),
                        duplicate,
                    ))
                })?
                .map_err(|e| format!("real menu registration: {e}"))?;
                let before = target
                    .lease
                    .snapshot()
                    .map_err(|e| format!("menu pre-release snapshot: {e:?}"))?;
                if before.retired
                    || before.binding.browser != handle.snapshot().browser
                    || before.binding.window != window_id
                    || !duplicate
                {
                    return Err("real menu View/window registration mismatch".into());
                }
                menu_probe = Some((
                    services,
                    target,
                    entity.downgrade(),
                    window_id,
                    before.revision,
                ));
            }
            let ready = options.gate.with_extension("ready");
            let temp = options.gate.with_extension("ready.tmp");
            std::fs::write(&temp, std::process::id().to_string()).map_err(|e| e.to_string())?;
            std::fs::rename(temp, ready).map_err(|e| e.to_string())?;
            let gate_deadline = Instant::now() + Duration::from_secs(40);
            while !options.gate.with_extension("detach").is_file() {
                if options.gate.is_file() {
                    return Err("observer released without validating embedded window".into());
                }
                if Instant::now() >= gate_deadline {
                    return Err("owned-window screenshot handshake timeout".into());
                }
                turn(cx, rt, receiver, false).await?;
                stats.check()?;
            }
            if let (Some(restart), Some(profile)) = (&options.restart, profile) {
                if std::fs::read_to_string(options.gate.with_extension("detach"))
                    .map_err(|e| e.to_string())?
                    .trim()
                    != std::process::id().to_string()
                {
                    return Err("restart storage acknowledgement PID mismatch".into());
                }
                let flushed = restart.complete_write(cx, rt, receiver, profile).await?;
                out.restart_detail = Some(restart.report(profile, flushed));
            }
            update(cx, root, |host, window, cx| {
                entity.update(cx, |view, cx| view.release_image(window, cx))?;
                host.browser = None;
                host.detached = true;
                cx.notify();
                Ok::<_, rust_cef_gpui::ViewError>(())
            })?
            .map_err(|e| format!("view removal: {e:?}"))?;
            out.released_view = Some(update(cx, root, |_, _, cx| entity.read(cx).diagnostics())?);
            Ok::<_, String>(())
        }
        .await;
        // Always remove the entity, even when validation failed. The handle remains
        // observable; its attachment Drop only requests graceful closure.
        let _ = update(cx, root, |host, _, cx| {
            host.browser = None;
            host.detached = true;
            cx.notify();
        });
        drop(entity);
        #[cfg(feature = "validation")]
        let result = if let Some(mut print_case) = print_case {
            let failed_before_retire = result.is_err();
            let result = match result {
                Ok(()) => print_case
                    .retire(
                        cx,
                        root,
                        rt,
                        receiver,
                        &handle,
                        options.print_cleanup_fault_probe
                            == Some(PrintCleanupFault::CloseAfterDetach),
                    )
                    .await
                    .map(|detail| out.print_confirmation_detail = Some(detail)),
                Err(primary) => Err(primary),
            };
            if let Err(primary) = &result {
                let cleanup = if failed_before_retire || print_case.owner_close_observed() {
                    print_case
                        .cleanup_after_failure(cx, root, rt, receiver, &handle)
                        .await
                } else {
                    Err("original close was not proved; no second wait or retirement claim".into())
                };
                let (detail, error) = match cleanup {
                    Ok(detail) => (detail, "null".into()),
                    Err(error) => ("null".into(), quote(&error)),
                };
                let _ = write_json(
                    &options
                        .report
                        .with_file_name("print-confirmation-cleanup.json"),
                    object(vec![
                        ("primary", quote(primary)),
                        ("cleanup", detail),
                        ("cleanup_error", error),
                    ]),
                );
            }
            result
        } else {
            result
        };
        if options.menu_services_probe && result.is_err() {
            // One best-effort final draw for this new probe's failure path:
            // release a previous draw-list's RenderImage owner before shutdown.
            // Never replace the primary observer failure or reset its counters.
            let cleanup = rendered(cx, root).await;
            let _ = write_json(
                &options.report.with_file_name("menu-services-failure.json"),
                object(vec![
                    ("primary", quote(result.as_ref().err().unwrap())),
                    (
                        "retirement_frame_error",
                        cleanup.err().map_or_else(|| "null".into(), |e| quote(&e)),
                    ),
                    ("success", "false".into()),
                ]),
            );
        }
        result?;
        rendered(cx, root).await?;
        if let Some((mut services, target, weak, window_id, before_revision)) = menu_probe {
            let after = target
                .lease
                .snapshot()
                .map_err(|e| format!("menu post-release snapshot: {e:?}"))?;
            let status = services
                .status(handle.snapshot().browser)
                .map_err(|e| format!("menu post-release status: {e}"))?;
            let weak_released = weak.upgrade().is_none();
            let delivery_closed = target.delivery.capture().is_err();
            if !weak_released
                || !after.retired
                || !after.is_drained()
                || status.pending
                || status.first_lease_error.is_some()
                || status.cancellation_error.is_some()
                || !delivery_closed
            {
                return Err("menu owner survived/revived released real View".into());
            }
            let receipt = services
                .unregister(handle.snapshot().browser)
                .map_err(|e| format!("menu unregister: {e}"))?;
            if !receipt.status().map_err(|e| e.to_string())?.lease.retired {
                return Err("menu cleanup receipt revived UI owner".into());
            }
            out.menu_services_verified = true;
            write_json(
                &options
                    .report
                    .with_file_name("menu-services-lifecycle.json"),
                object(vec![
                    ("schema", "1".into()),
                    ("pid", std::process::id().to_string()),
                    ("success", "true".into()),
                    ("scope", quote("real-view-entity-release-only")),
                    ("window_id", window_id.to_string()),
                    (
                        "browser",
                        format!(
                            "[{},{}]",
                            after.binding.browser.runtime().value(),
                            after.binding.browser.local()
                        ),
                    ),
                    ("before_revision", before_revision.to_string()),
                    ("before_retired", "false".into()),
                    ("after_retired", bool_json(after.retired)),
                    ("in_flight", after.in_flight.to_string()),
                    ("weak_entity_released", bool_json(weak_released)),
                    ("duplicate_registration_refused", "true".into()),
                    ("delivery_capture_rejected", bool_json(delivery_closed)),
                    ("cleanup_receipt_retired", "true".into()),
                    ("native_menu_callbacks_tested", "false".into()),
                    ("menu_focus_widget_tested", "false".into()),
                    ("selection_execution_tested", "false".into()),
                ]),
            )?;
        }
        let alive = update(cx, root, |host, _, _| {
            host.browser.is_none() && host.detached && host.renders > 1
        })?;
        if !alive {
            return Err("host window did not remain alive after View removal".into());
        }
        out.detached_alive = true;
        let close_deadline = Instant::now() + Duration::from_secs(10);
        while !matches!(
            handle.snapshot().state,
            rust_cef_gpui::contracts::SessionState::Closing
                | rust_cef_gpui::contracts::SessionState::Closed
        ) {
            if Instant::now() >= close_deadline {
                return Err("View release did not request graceful close".into());
            }
            turn(cx, rt, receiver, false).await?;
        }
        // This observation is not an independent native close-mode audit.
        // The original-handle Graceful post has its own registry regression.
        out.view_release_observed_closing = true;
        std::fs::write(
            options.gate.with_extension("detached"),
            std::process::id().to_string(),
        )
        .map_err(|e| e.to_string())?;
        while !options.gate.is_file() {
            if Instant::now() >= close_deadline {
                return Err("host-alive screenshot handshake timeout".into());
            }
            turn(cx, rt, receiver, false).await?;
            stats.check()?;
        }
        Ok(())
    }
    async fn scenario(
        cx: &mut AsyncApp,
        mut rt: Runtime,
        receiver: UiWakeReceiver,
        stats: Arc<Stats>,
        root: WindowHandle<HostRoot>,
        options: Options,
    ) -> Outcome {
        let mut out = Outcome::default();
        #[cfg(feature = "validation")]
        {
            out.window_source_enabled = options.timing_window_source_pressure;
        }
        out.error = match options.scenario {
            HostScenario::Embed => body(cx, &mut rt, &receiver, &stats, root, &options, &mut out)
                .await
                .err(),
            HostScenario::Dispatch | HostScenario::ProfileDispatch => {
                dispatch::body(cx, &mut rt, &receiver, &stats, root, &options, &mut out)
                    .await
                    .err()
            }
            HostScenario::PaintLatency => {
                #[cfg(feature = "validation")]
                {
                    paint_latency::body(cx, &mut rt, &receiver, &stats, root, &options, &mut out)
                        .await
                        .err()
                }
                #[cfg(not(feature = "validation"))]
                {
                    Some("PaintLatency requires validation feature".into())
                }
            }
            HostScenario::Popup => {
                popup::body(cx, &mut rt, &receiver, &stats, root, &options, &mut out)
                    .await
                    .err()
            }
            HostScenario::Input | HostScenario::ImeObserve => {
                input::body(cx, &mut rt, &receiver, &stats, root, &options, &mut out)
                    .await
                    .err()
            }
            HostScenario::Resize => {
                resize::body(cx, &mut rt, &receiver, &stats, root, &options, &mut out)
                    .await
                    .err()
            }
            HostScenario::Unfocused => {
                unfocused::body(cx, &mut rt, &receiver, &stats, root, &options, &mut out)
                    .await
                    .err()
            }
            HostScenario::Exhaustion => {
                #[cfg(feature = "validation")]
                {
                    exhaustion::body(cx, &mut rt, &receiver, &stats, root, &options, &mut out)
                        .await
                        .err()
                }
                #[cfg(not(feature = "validation"))]
                {
                    Some("Exhaustion requires validation feature".into())
                }
            }
            HostScenario::MenuDefault => {
                #[cfg(feature = "validation")]
                {
                    menu_default::body(cx, &mut rt, &receiver, &stats, root, &options, &mut out)
                        .await
                        .err()
                }
                #[cfg(not(feature = "validation"))]
                {
                    Some("MenuDefault requires validation feature".into())
                }
            }
            HostScenario::FindView => {
                #[cfg(feature = "validation")]
                {
                    find_view::body(cx, &mut rt, &receiver, &stats, root, &options, &mut out)
                        .await
                        .err()
                }
                #[cfg(not(feature = "validation"))]
                {
                    Some("FindView requires validation feature".into())
                }
            }
            HostScenario::SourceViewer => {
                #[cfg(feature = "validation")]
                {
                    source_viewer::body(cx, &mut rt, &receiver, &stats, root, &options, &mut out)
                        .await
                        .err()
                }
                #[cfg(not(feature = "validation"))]
                {
                    Some("SourceViewer requires validation feature".into())
                }
            }
            HostScenario::Visibility => {
                #[cfg(feature = "validation")]
                {
                    visibility_scene::body(cx, &mut rt, &receiver, &stats, root, &options, &mut out)
                        .await
                        .err()
                }
                #[cfg(not(feature = "validation"))]
                {
                    Some("Visibility requires validation feature".into())
                }
            }
        };
        if options.scenario != HostScenario::Embed {
            // A Popup window may already be genuinely destroyed. Only route
            // cleanup to handles still in the actual App window set.
            let roots: Vec<_> = std::iter::once(root)
                .chain(out.additional_roots.iter().copied())
                .collect();
            for owned_root in roots {
                let live = app_update(cx, |app| app.windows().contains(&owned_root.into()));
                match live {
                    Ok(false) => continue,
                    Err(error) => {
                        out.error.get_or_insert(error);
                        continue;
                    }
                    Ok(true) => {}
                }
                let cleanup = update(cx, owned_root, |host, window, cx| {
                    let mut errors = Vec::new();
                    for entity in [host.browser.take(), host.secondary.take()]
                        .into_iter()
                        .flatten()
                    {
                        if let Err(error) =
                            entity.update(cx, |view, cx| view.release_image(window, cx))
                        {
                            errors.push(format!("cleanup image: {error:?}"));
                        }
                    }
                    host.detached = true;
                    host.secondary_detached = true;
                    cx.notify();
                    errors
                });
                match cleanup {
                    Ok(errors) => {
                        if let Some(error) = errors.into_iter().next() {
                            out.error.get_or_insert(error);
                        }
                    }
                    Err(error) => {
                        out.error.get_or_insert(error);
                    }
                }
                if let Err(error) = rendered(cx, owned_root).await {
                    out.error.get_or_insert(error);
                }
            }
        }
        let ticket =
            rt.request_shutdown(CloseMode::Force, Instant::now() + Duration::from_secs(15));
        let until = Instant::now() + Duration::from_secs(15);
        loop {
            #[cfg(feature = "validation")]
            {
                if (out.window_source.is_some()
                    || out.window_flush.is_some()
                    || out.window_print.is_some())
                    && APP_BORROW.with(Cell::get)
                {
                    out.error
                        .get_or_insert("window service retirement inside GPUI borrow".into());
                    break;
                }
                if out.error.is_some()
                    && let Some(print) = out.window_print.as_mut()
                    && let Err(error) = print.cleanup_closed(cx, &rt).await
                {
                    eprintln!("original print panel cleanup: {error}");
                }
                let proof = out.error.is_none();
                let source_drained = if let Some(source) = out.window_source.as_mut() {
                    match source.drain_turn(&mut rt, &ticket, out.window_flush.as_ref(), proof) {
                        Ok(ready) => ready,
                        Err(e) => {
                            out.error.get_or_insert(e);
                            false
                        }
                    }
                } else {
                    true
                };
                // Only the success proof requires Source before the original
                // flush. Failure cleanup may release each exact owned resource.
                if (source_drained || out.error.is_some())
                    && let Some(flush) = out.window_flush.as_mut()
                    && !flush.attempted()
                {
                    if let Err(e) = flush.drain_turn(&mut rt, &ticket, out.error.is_none()) {
                        out.error.get_or_insert(e);
                    }
                }
            }
            match rt.poll_shutdown(&ticket) {
                ShutdownStatus::Ready => {
                    if APP_BORROW.with(Cell::get) {
                        out.error.get_or_insert("finish inside GPUI borrow".into());
                        break;
                    }
                    if rt.finish_shutdown().is_ok() {
                        out.finished = true;
                        break;
                    }
                }
                ShutdownStatus::Stopped => {
                    out.finished = true;
                    break;
                }
                _ => {}
            }
            if Instant::now() >= until {
                out.error.get_or_insert(
                    "shutdown barrier timeout; no early CEF shutdown authorized".into(),
                );
                break;
            }
            if let Err(e) = turn(cx, &mut rt, &receiver, false).await {
                out.error.get_or_insert(e);
                break;
            }
        }
        if let Err(e) = stats.check() {
            out.error.get_or_insert(e);
        }
        #[cfg(feature = "validation")]
        if out.finished
            && let Some(flush) = &out.window_flush
        {
            if options.timing_extra_flush_fault {
                // Emit failure cleanup only after the genuine original ticket
                // completed; never turn the intentional failure into success.
                out.window_flush_cleanup_detail = Some(
                    flush
                        .cleanup_report(&rt)
                        .unwrap_or_else(|e| object(vec![("error", quote(&e))])),
                );
            } else if out.error.is_none() {
                match flush.report(&rt) {
                    Ok(detail) => out.window_flush_detail = Some(detail),
                    Err(e) => {
                        out.error.get_or_insert(e);
                    }
                }
            }
        }
        #[cfg(feature = "validation")]
        if out.finished
            && out.error.is_none()
            && let Some(source) = &out.window_source
        {
            match source.report(&rt) {
                Ok(detail) => out.window_source_detail = Some(detail),
                Err(e) => {
                    out.error.get_or_insert(e);
                }
            }
        }
        if out.finished
            && let Some(browser) = out.browser
        {
            let mut total = 0usize;
            for browser in std::iter::once(browser).chain(out.additional_browsers.iter().copied()) {
                match rt.frame_budget_stats(browser) {
                    Ok(budget) => total += budget.used,
                    Err(error) => {
                        out.error.get_or_insert(format!("final budget: {error:?}"));
                    }
                }
            }
            out.budget_used = Some(total);
            if total != 0 {
                out.error.get_or_insert(format!(
                    "pixel owners still charged after detach/shutdown: {total}"
                ));
            }
        }
        if out.finished && options.scenario == HostScenario::PaintLatency {
            let retired = timing_retired(&rt, out.browser);
            match retired {
                Ok(report) => out.paint_timing_retired = Some(report),
                Err(error) => {
                    out.error.get_or_insert(error);
                }
            }
            if options.timing_profile_peer {
                let peer = match out.additional_browsers.as_slice() {
                    [id] => Some(*id),
                    _ => None,
                };
                match timing_retired(&rt, peer) {
                    Ok(report) => out.paint_timing_peer_retired = Some(report),
                    Err(error) => {
                        out.error.get_or_insert(error);
                    }
                }
            }
        }
        out.diag = Some(rt.diagnostics());
        if !out.finished {
            std::mem::forget(rt);
        }
        out
    }

    fn timing_retired(rt: &Runtime, browser: Option<BrowserId>) -> Result<String, String> {
        let browser = browser.ok_or("timing retirement without the original Browser")?;
        match (
            rt.frame_capture_diagnostics(browser),
            rt.frame_budget_stats(browser),
        ) {
            (Ok(capture), Ok(budget))
                if capture.retired
                    && !capture.abandoned
                    && !capture.native_resources_live
                    && capture.live_imports == 0
                    && capture.staging_bytes == 0
                    && capture.maps == capture.unmaps
                    && capture.failures == 0
                    && capture.retirement_thread == capture.owner_thread
                    && Some(capture.owner_thread) == rt.diagnostics().cef_ui_thread_id
                    && budget.used == 0
                    && budget.peak <= budget.limit =>
            {
                Ok(object(vec![
                    ("retirement_thread", capture.retirement_thread.to_string()),
                    ("maps", capture.maps.to_string()),
                    ("unmaps", capture.unmaps.to_string()),
                    ("native_resources_live", "false".into()),
                    ("live_imports", capture.live_imports.to_string()),
                    ("staging_bytes", capture.staging_bytes.to_string()),
                    ("peak_staging_bytes", capture.staging_peak.to_string()),
                    ("cpu_bytes", budget.used.to_string()),
                    ("peak_cpu_bytes", budget.peak.to_string()),
                    ("cpu_limit_bytes", budget.limit.to_string()),
                ]))
            }
            other => Err(format!(
                "timing capture retirement mismatch {browser:?}: {other:?}"
            )),
        }
    }
    fn report(options: &Options, out: &Outcome, stats: &Stats) -> String {
        let mut fields = vec![
            ("schema", "1".into()),
            ("kind", quote("component-host")),
            (
                "passive_distribution",
                bool_json(options.passive_distribution),
            ),
            (
                "menu_default",
                out.menu_default_detail
                    .clone()
                    .unwrap_or_else(|| "null".into()),
            ),
            ("find_view_verified", bool_json(out.find_view_verified)),
            (
                "find_view",
                out.find_view_detail
                    .clone()
                    .unwrap_or_else(|| "null".into()),
            ),
            (
                "source_viewer_verified",
                bool_json(out.source_viewer_verified),
            ),
            (
                "source_viewer",
                out.source_viewer_detail
                    .clone()
                    .unwrap_or_else(|| "null".into()),
            ),
            (
                "menu_services_probe",
                bool_json(options.menu_services_probe),
            ),
            (
                "menu_services_verified",
                bool_json(out.menu_services_verified),
            ),
            (
                "scenario",
                quote(match options.scenario {
                    HostScenario::Embed => "Embed",
                    HostScenario::Dispatch => "Dispatch",
                    HostScenario::ProfileDispatch => "ProfileDispatch",
                    HostScenario::PaintLatency => "PaintLatency",
                    HostScenario::Popup => "Popup",
                    HostScenario::Input => "Input",
                    HostScenario::ImeObserve => "ImeObserve",
                    HostScenario::Resize => "Resize",
                    HostScenario::Unfocused => "Unfocused",
                    HostScenario::Exhaustion => "Exhaustion",
                    HostScenario::Visibility => "Visibility",
                    HostScenario::SourceViewer => "SourceViewer",
                    HostScenario::FindView => "FindView",
                    HostScenario::MenuDefault => "MenuDefault",
                }),
            ),
            (
                "dispatch",
                out.dispatch_detail.clone().unwrap_or_else(|| "null".into()),
            ),
            (
                "paint_timing",
                out.paint_timing_detail
                    .clone()
                    .unwrap_or_else(|| "null".into()),
            ),
            (
                "paint_timing_retired",
                out.paint_timing_retired
                    .clone()
                    .unwrap_or_else(|| "null".into()),
            ),
            (
                "paint_timing_partial",
                if out.paint_timing_detail.is_some() {
                    "null".into()
                } else {
                    format!("[{}]", out.paint_timing_rows.join(","))
                },
            ),
            (
                "paint_timing_peer_partial",
                if out.paint_timing_peer.is_some() || out.paint_timing_peer_rows.is_empty() {
                    "null".into()
                } else {
                    format!("[{}]", out.paint_timing_peer_rows.join(","))
                },
            ),
            (
                "popup",
                out.popup_detail.clone().unwrap_or_else(|| "null".into()),
            ),
            (
                "input",
                out.input_detail.clone().unwrap_or_else(|| "null".into()),
            ),
            (
                "resize",
                out.resize_detail.clone().unwrap_or_else(|| "null".into()),
            ),
            (
                "unfocused",
                out.unfocused_detail
                    .clone()
                    .unwrap_or_else(|| "null".into()),
            ),
            (
                "exhaustion",
                out.exhaustion_detail
                    .clone()
                    .unwrap_or_else(|| "null".into()),
            ),
            (
                "visibility",
                out.visibility_detail
                    .clone()
                    .unwrap_or_else(|| "null".into()),
            ),
            (
                "mode",
                quote(if options.mode == ThreadMode::Integrated {
                    "Integrated"
                } else {
                    "Threaded"
                }),
            ),
            (
                "backend",
                quote(if options.backend == RenderPreference::SoftwareRequired {
                    "Software"
                } else {
                    "D3d11"
                }),
            ),
            ("pid", std::process::id().to_string()),
            (
                "error",
                out.error
                    .as_ref()
                    .map_or_else(|| "null".into(), |e| quote(e)),
            ),
            ("created", stats.created.load(Ordering::Acquire).to_string()),
            ("closed", stats.closed.load(Ordering::Acquire).to_string()),
            (
                "valid_frames",
                stats.valid_frames.load(Ordering::Acquire).to_string(),
            ),
            ("ffi", stats.ffi.load(Ordering::Acquire).to_string()),
            (
                "borrow_violations",
                stats.borrow_violations.load(Ordering::Acquire).to_string(),
            ),
            ("wake_consumed", out.wake_consumed.to_string()),
            ("host_alive_after_view_drop", bool_json(out.detached_alive)),
            (
                "view_release_observed_closing",
                bool_json(out.view_release_observed_closing),
            ),
            (
                "pressure_closed_before_view_release",
                bool_json(out.pressure_closed_before_view_release),
            ),
            (
                "duplicate_attachment_refused",
                bool_json(out.duplicate_refused),
            ),
            ("finished", bool_json(out.finished)),
            (
                "final_cpu_used",
                out.budget_used
                    .map_or_else(|| "null".into(), |v| v.to_string()),
            ),
            (
                "released_view",
                out.released_view
                    .as_ref()
                    .map_or_else(|| "null".into(), metrics_json),
            ),
            (
                "paint",
                out.paint
                    .as_ref()
                    .map_or_else(|| "null".into(), metrics_json),
            ),
        ];
        if options.timing_profile_peer {
            fields.extend([
                (
                    "paint_timing_peer",
                    out.paint_timing_peer
                        .clone()
                        .unwrap_or_else(|| "null".into()),
                ),
                (
                    "paint_timing_peer_retired",
                    out.paint_timing_peer_retired
                        .clone()
                        .unwrap_or_else(|| "null".into()),
                ),
                (
                    "window_flush",
                    out.window_flush_detail
                        .clone()
                        .unwrap_or_else(|| "null".into()),
                ),
                (
                    "window_flush_cleanup",
                    out.window_flush_cleanup_detail
                        .clone()
                        .unwrap_or_else(|| "null".into()),
                ),
                (
                    "window_source",
                    out.window_source_detail
                        .clone()
                        .unwrap_or_else(|| "null".into()),
                ),
            ]);
        }
        if options.print_confirmation_probe {
            fields.push((
                "print_confirmation",
                out.print_confirmation_detail
                    .clone()
                    .unwrap_or_else(|| "null".into()),
            ));
        }
        if options.restart.is_some() {
            fields.push((
                "profile_restart",
                out.restart_detail.clone().unwrap_or_else(|| "null".into()),
            ));
        }
        if let Some(d) = out.diag.as_ref() {
            fields.extend([
                ("main_thread", d.main_thread_id.to_string()),
                ("init_thread", d.init_thread_id.to_string()),
                ("cef_ui_thread", optional_number(d.cef_ui_thread_id)),
                ("shutdown_thread", optional_number(d.shutdown_thread_id)),
                ("manual_pump", d.manual_pump_count.to_string()),
                (
                    "observed_backend",
                    quote(&format!("{:?}", d.observed_backend)),
                ),
                ("tasks_posted", d.tasks.posted.to_string()),
                ("tasks_completed", d.tasks.completed.to_string()),
                ("post_failures", d.tasks.post_failed.to_string()),
                ("pending_tasks", d.tasks.pending.to_string()),
                ("pending_requests", d.pending_requests.to_string()),
                ("pending_profiles", d.pending_profiles.to_string()),
                (
                    "pending_cookie_flushes",
                    d.pending_cookie_flushes.to_string(),
                ),
                (
                    "cookie_flushes_accepted",
                    d.cookie_flushes_accepted.to_string(),
                ),
                (
                    "cookie_flushes_completed",
                    d.cookie_flushes_completed.to_string(),
                ),
                ("profile_refs", d.profile_references_retained.to_string()),
                ("app_retained", bool_json(d.app_reference_retained)),
                ("root_lease", bool_json(d.root_lease_held)),
                ("shutdown_calls", d.cef_shutdown_calls.to_string()),
                ("native_after", d.native_calls_after_shutdown.to_string()),
            ]);
        }
        let ok = out.error.is_none()
            && (!options.menu_services_probe || out.menu_services_verified)
            && (!options.print_confirmation_probe || out.print_confirmation_detail.is_some())
            && (options.restart.is_none() || out.restart_detail.is_some())
            && (options.scenario != HostScenario::PaintLatency
                || out.paint_timing_detail.is_some())
            && (!options.timing_profile_peer
                || (out.paint_timing_peer.is_some() && out.paint_timing_peer_retired.is_some()))
            && (options.scenario != HostScenario::SourceViewer || out.source_viewer_verified)
            && (options.scenario != HostScenario::FindView || out.find_view_verified)
            && out.finished
            && out.detached_alive
            && close_order_verified(options, out)
            && out.duplicate_refused
            && out.paint.as_ref().is_some_and(|d| d.paint_calls > 0)
            && stats.created.load(Ordering::Acquire)
                == (if options.timing_profile_peer {
                    2
                } else if matches!(
                    options.scenario,
                    HostScenario::Embed
                        | HostScenario::PaintLatency
                        | HostScenario::Resize
                        | HostScenario::Unfocused
                        | HostScenario::Exhaustion
                        | HostScenario::Visibility
                        | HostScenario::SourceViewer
                        | HostScenario::FindView
                        | HostScenario::MenuDefault
                ) {
                    1
                } else {
                    2
                })
            && stats.closed.load(Ordering::Acquire) == stats.created.load(Ordering::Acquire)
            && stats.borrow_violations.load(Ordering::Acquire) == 0;
        fields.push(("success", bool_json(ok)));
        object(fields)
    }
    fn close_order_verified(options: &Options, out: &Outcome) -> bool {
        if options.timing_window_pressure {
            out.pressure_closed_before_view_release && !out.view_release_observed_closing
        } else {
            out.view_release_observed_closing && !out.pressure_closed_before_view_release
        }
    }

    // Explicit MenuDefault diagnostics only: no global logger/handler, new
    // activation path, success assertion, or inferred completion on an AV.
    fn menu_startup_cut(options: &Options, name: &str) -> Result<(), String> {
        #[cfg(feature = "validation")]
        if options.scenario == HostScenario::MenuDefault {
            return menu_default::step(options, name, object(vec![]));
        }
        let _ = (options, name);
        Ok(())
    }
    fn timing_cache_root(bundle: &Path, mode: ThreadMode, backend: RenderPreference) -> PathBuf {
        bundle
            .parent()
            .expect("owned timing bundle parent")
            .join("paint-fixture-cache")
            .join(format!("{mode:?}-{backend:?}"))
    }

    pub fn run() -> i32 {
        let options = match parse() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{e}");
                return 2;
            }
        };
        if let Err(e) = menu_startup_cut(&options, "parsed") {
            eprintln!("menu startup cut: {e}");
            return 2;
        }
        #[cfg(feature = "validation")]
        if matches!(
            options.scenario,
            HostScenario::SourceViewer | HostScenario::FindView | HostScenario::MenuDefault
        ) {
            if let Err(e) = visibility::require_unlocked_session() {
                eprintln!("{e}");
                return 2;
            }
        }
        if let Err(e) = menu_startup_cut(&options, "session-qualified") {
            eprintln!("menu startup cut: {e}");
            return 2;
        }
        let stats = Arc::new(Stats::default());
        let outcome = Arc::new(Mutex::new(Outcome::default()));
        let app_out = outcome.clone();
        let app_stats = stats.clone();
        let app_options = options.clone();
        if let Err(e) = menu_startup_cut(&options, "before-application") {
            eprintln!("menu startup cut: {e}");
            return 2;
        }
        gpui_platform::application().run(move |cx| {
            let initialized = (|| -> Result<(), String> {
                menu_startup_cut(&app_options, "application-entered")?;
                if app_options.scenario == HostScenario::MenuDefault {
                    rust_cef_gpui::init_default_menu_ui(cx);
                    menu_startup_cut(&app_options, "ui-initialized")?;
                }
                menu_startup_cut(&app_options, "before-open-window")
            })();
            if let Err(e) = initialized {
                app_out.lock().unwrap().error = Some(format!("menu startup cut: {e}"));
                cx.quit();
                return;
            }
            let right_bounds = if app_options.resize_passive_right {
                let bounds = cx
                    .primary_display()
                    .ok_or("passive Resize primary work area missing")
                    .and_then(|display| resize::passive_right_bounds(display.visible_bounds()));
                match bounds {
                    Ok(bounds) => Some(bounds),
                    Err(error) => {
                        app_out.lock().unwrap().error = Some(error.into());
                        cx.quit();
                        return;
                    }
                }
            } else {
                None
            };
            let may_close = Arc::new(AtomicBool::new(false));
            let guard = may_close.clone();
            let root = match cx.open_window(
                WindowOptions {
                    // Passive diagnostics never request foreground activation.
                    focus: !app_options.passive_distribution
                        && !app_options.exhaustion_manual_foreground
                        && !app_options.visibility_manual_foreground
                        && !app_options.resize_manual_foreground
                        && !app_options.resize_passive
                        && !matches!(
                            app_options.scenario,
                            HostScenario::MenuDefault
                                | HostScenario::ProfileDispatch
                                | HostScenario::PaintLatency
                                | HostScenario::SourceViewer
                                | HostScenario::FindView
                        ),
                    window_bounds: Some(WindowBounds::Windowed(
                        if let Some(bounds) = right_bounds {
                            bounds
                        } else if matches!(
                            app_options.scenario,
                            HostScenario::Exhaustion | HostScenario::Visibility
                        ) {
                            Bounds::new(gpui::point(px(40.), px(40.)), size(px(600.), px(450.)))
                        } else if app_options.resize_manual_foreground || app_options.resize_passive
                        {
                            // Start at the Resize stage-0 client target without SetWindowPos.
                            let (w, h) = resize::initial_client_size();
                            Bounds::new(
                                gpui::point(px(40.), px(40.)),
                                size(px(w as f32), px(h as f32)),
                            )
                        } else if app_options.scenario == HostScenario::MenuDefault {
                            Bounds::new(gpui::point(px(40.), px(40.)), size(px(380.), px(600.)))
                        } else if app_options.scenario == HostScenario::FindView {
                            // Initial placement of OUR NEW fixture only. Keep the
                            // full 320x240 browser away from observed foreign UI;
                            // no existing/foreign window is moved or activated.
                            Bounds::new(gpui::point(px(40.), px(40.)), size(px(380.), px(400.)))
                        } else if matches!(
                            app_options.scenario,
                            HostScenario::Unfocused | HostScenario::SourceViewer
                        ) {
                            Bounds::new(gpui::point(px(40.), px(40.)), size(px(600.), px(400.)))
                        } else if matches!(
                            app_options.scenario,
                            HostScenario::ProfileDispatch
                                | HostScenario::PaintLatency
                                | HostScenario::Popup
                                | HostScenario::Input
                                | HostScenario::ImeObserve
                                | HostScenario::Resize
                        ) {
                            // Position only our fixture away from the observed
                            // foreign right-side window; never move/hide that app.
                            Bounds::new(gpui::point(px(40.), px(40.)), size(px(680.), px(440.)))
                        } else {
                            Bounds::centered(None, size(px(680.), px(440.)), cx)
                        },
                    )),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some(TITLE.into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                move |window, cx| {
                    window.on_window_should_close(cx, move |_, _| guard.load(Ordering::Acquire));
                    cx.new(|_| HostRoot {
                        browser: None,
                        secondary: None,
                        multi: false,
                        secondary_detached: false,
                        extent: (320., 240.),
                        renders: 0,
                        detached: false,
                        fault_label: None,
                        menu: None,
                        print_confirmation: None,
                    })
                },
            ) {
                Ok(v) => v,
                Err(e) => {
                    app_out.lock().unwrap().error = Some(format!("window: {e}"));
                    cx.quit();
                    return;
                }
            };
            if let Err(e) = menu_startup_cut(&app_options, "window-opened") {
                app_out.lock().unwrap().error = Some(format!("menu startup cut: {e}"));
                may_close.store(true, Ordering::Release);
                cx.quit();
                return;
            }
            let out = app_out.clone();
            let counters = app_stats.clone();
            let opts = app_options.clone();
            cx.spawn(async move |cx: &mut AsyncApp| {
                let (sender, receiver) = dispatcher::channel();
                let services = RuntimeServices {
                    events: Arc::new(Events(counters.clone())),
                    frames: Arc::new(Frames(counters.clone())),
                    ui: Arc::new(Wake {
                        sender,
                        stats: counters.clone(),
                    }),
                };
                let executable = std::env::current_exe().expect("host executable");
                let directory = executable.parent().expect("staged host directory");
                let root_cache_path = if let Some(restart) = &opts.restart {
                    restart.root.clone()
                } else if opts.scenario == HostScenario::PaintLatency {
                    // The driver owns a new bundle parent per run. Keep each
                    // cell's cache separate on that volume, outside its bundle.
                    timing_cache_root(directory, opts.mode, opts.backend)
                } else {
                    opts.report.parent().unwrap().join("cache")
                };
                let config = RuntimeConfig {
                    thread_mode: opts.mode,
                    files: RuntimeFiles {
                        directory: directory.to_path_buf(),
                        helper_executable: opts.helper.clone(),
                    },
                    root_cache_path,
                    default_profile: ProfileStorage::Memory,
                    content_policy: opts.restart.as_ref().map_or_else(
                        || TrustedContentPolicy {
                            allowed_origins: vec![],
                            allow_embedded_data: true,
                        },
                        distribution_restart::Restart::policy,
                    ),
                    settings: Default::default(),
                };
                if let Err(e) = menu_startup_cut(&opts, "before-runtime-start") {
                    out.lock().unwrap().error = Some(format!("menu startup cut: {e}"));
                    may_close.store(true, Ordering::Release);
                    cx.update(|cx| cx.quit());
                    return;
                }
                let result = match Runtime::start(config, services) {
                    Ok(rt) => scenario(cx, rt, receiver, counters, root, opts).await,
                    Err(e) => {
                        let message = format!("start: {e:?}");
                        let fatal_exit = match e {
                            StartError::ProcessExitRequired { exit_code }
                            | StartError::InitializeFailed { exit_code } => {
                                Some(exit_code.unwrap_or(1))
                            }
                            _ => None,
                        };
                        Outcome {
                            error: Some(message),
                            fatal_exit,
                            ..Default::default()
                        }
                    }
                };
                *out.lock().unwrap() = result;
                may_close.store(true, Ordering::Release);
                cx.update(|cx| cx.quit());
            })
            .detach();
            if !app_options.passive_distribution
                && !app_options.exhaustion_manual_foreground
                && !app_options.visibility_manual_foreground
                && !app_options.resize_manual_foreground
                && !app_options.resize_passive
                && !matches!(
                    app_options.scenario,
                    HostScenario::MenuDefault
                        | HostScenario::ProfileDispatch
                        | HostScenario::PaintLatency
                )
            {
                cx.activate(true);
            }
        });
        let out = outcome.lock().unwrap();
        let text = report(&options, &out, &stats);
        let good = out.error.is_none()
            && out.finished
            && out.detached_alive
            && close_order_verified(&options, &out);
        if let Err(e) = write_json(&options.report, text) {
            eprintln!("report: {e}");
            return 1;
        }
        if let Some(code) = out.fatal_exit {
            code
        } else if good {
            0
        } else {
            1
        }
    }
}
