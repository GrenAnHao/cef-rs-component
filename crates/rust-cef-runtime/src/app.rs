//! CEF App/BrowserProcessHandler wiring: process callbacks only record owned data.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::sync::{
    Arc, Weak,
    atomic::{AtomicU8, Ordering},
};

use cef::*;
use rust_cef_core::contracts::{CliSwitch, validate_allowlisted_switch};
use rust_cef_core::custom_schemes::{CUSTOM_SCHEMES_SWITCH, CustomSchemeConfig};

use crate::{executor::current_thread_id, profiles::ProfileManager, runtime::StartupEndpoint};

/// Observed `--single-process` state recorded by the real CEF command line.
/// 0 = never observed, 1 = observed absent (disabled), 2 = observed present.
pub(crate) type SingleProcessObservation = Arc<AtomicU8>;

pub(crate) const SINGLE_PROCESS_UNOBSERVED: u8 = 0;
pub(crate) const SINGLE_PROCESS_DISABLED: u8 = 1;
pub(crate) const SINGLE_PROCESS_ENABLED: u8 = 2;

wrap_app! {
    pub(crate) struct RuntimeAppBuilder {
        startup: Arc<StartupEndpoint>,
        // Weak on purpose: the App must never keep the profile table (and its
        // CEF references) alive past the runtime's normal release.
        profiles: Weak<ProfileManager>,
        single_process: SingleProcessObservation,
        allowlisted_switches: Vec<CliSwitch>,
        custom_schemes: Vec<CustomSchemeConfig>,
        custom_schemes_switch: Option<String>,
    }

    impl App {
        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            let Some(command_line) = command_line else {
                return;
            };
            // Never accept untrusted external switch overrides for custom schemes.
            command_line.remove_switch(Some(&CefString::from(CUSTOM_SCHEMES_SWITCH)));
            // Record only what CEF actually parsed. This is the evidence the
            // diagnostics report; our own configuration is never substituted.
            let absent = command_line.has_switch(Some(&CefString::from("single-process"))) == 0;
            self.single_process.store(
                if absent {
                    SINGLE_PROCESS_DISABLED
                } else {
                    SINGLE_PROCESS_ENABLED
                },
                Ordering::Release,
            );
            // Off-screen browsers never need a startup window.
            command_line.append_switch(Some(&CefString::from("no-startup-window")));
            // CEF only routes HTTP auth challenges to RequestHandler::GetAuthCredentials
            // when the built-in Chrome login prompt is disabled (CefSharp OffScreen/Wpf
            // CefSettings.cs -> cef/issues/3603). Appending unconditionally preserves
            // zero unmanaged Chrome auth modals for both default-deny and opt-in AuthService.
            command_line.append_switch(Some(&CefString::from("disable-chrome-login-prompt")));
            for switch in &self.allowlisted_switches {
                if validate_allowlisted_switch(switch).is_err() {
                    continue;
                }
                let name = CefString::from(switch.name.as_str());
                match &switch.value {
                    Some(value) => {
                        command_line.append_switch_with_value(
                            Some(&name),
                            Some(&CefString::from(value.as_str())),
                        );
                    }
                    None => {
                        command_line.append_switch(Some(&name));
                    }
                }
            }
            // Validation escape hatch, off unless the environment asks for it: a
            // windowed auxiliary (DevTools) needs a GPU shared context that some
            // hosts cannot create, and the failure takes the whole GPU process
            // down. Software compositing keeps the run alive; the default path
            // is untouched.
            // Validation escape hatch, off unless the environment asks for it.
            // "1"/"true" disables the GPU process outright; "swiftshader" keeps the
            // GPU process but makes ANGLE software-only, which is what a windowed
            // auxiliary needs when the host already holds the real adapter.
            if std::env::var_os("GPUI_CEF_ENABLE_SPELLING").is_some() {
                command_line.append_switch(Some(&CefString::from("enable-spelling")));
            }
            match std::env::var("GPUI_CEF_DISABLE_GPU").ok().as_deref() {
                None => {}
                Some("in-process") => {
                    command_line.append_switch(Some(&CefString::from("in-process-gpu")));
                }
                Some("swiftshader") => {
                    command_line.append_switch(Some(&CefString::from("use-angle")));
                    command_line.append_switch_with_value(
                        Some(&CefString::from("use-angle")),
                        Some(&CefString::from("swiftshader")),
                    );
                }
                Some(_) => {
                    command_line.append_switch(Some(&CefString::from("disable-gpu")));
                    command_line.append_switch(Some(&CefString::from("disable-gpu-compositing")));
                }
            }
        }

        fn on_register_custom_schemes(&self, registrar: Option<&mut SchemeRegistrar>) {
            let Some(registrar) = registrar else {
                return;
            };
            for scheme in &self.custom_schemes {
                let name = CefString::from(scheme.scheme.as_str());
                let _ = registrar.add_custom_scheme(Some(&name), scheme.options.to_cef_flags());
            }
        }

        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(RuntimeBrowserProcessHandler::new(
                self.startup.clone(),
                self.profiles.clone(),
                self.custom_schemes_switch.clone(),
            ))
        }
    }
}

wrap_browser_process_handler! {
    pub(crate) struct RuntimeBrowserProcessHandler {
        startup: Arc<StartupEndpoint>,
        profiles: Weak<ProfileManager>,
        custom_schemes_switch: Option<String>,
    }

    impl BrowserProcessHandler {
        fn on_before_child_process_launch(&self, command_line: Option<&mut CommandLine>) {
            let Some(command_line) = command_line else {
                return;
            };
            command_line.remove_switch(Some(&CefString::from(CUSTOM_SCHEMES_SWITCH)));
            if let Some(encoded) = &self.custom_schemes_switch
                && !encoded.is_empty()
            {
                command_line.append_switch_with_value(
                    Some(&CefString::from(CUSTOM_SCHEMES_SWITCH)),
                    Some(&CefString::from(encoded.as_str())),
                );
            }
        }

        fn on_context_initialized(&self) {
            // Persistent selects the global context; Memory bootstraps its own
            // shared incognito context. The process callback is not sufficient
            // for Memory readiness: Runtime also waits for its handler marker.
            let Some(profiles) = self.profiles.upgrade() else {
                return;
            };
            if profiles.default_context_initialized() {
                // Record only the endpoint identity; the wake happens inside
                // StartupEndpoint after its state lock is released.
                self.startup.context_initialized(current_thread_id());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::StartupEndpoint;
    use rust_cef_core::custom_schemes::{CustomSchemeOptions, encode_custom_schemes_switch};
    use rust_cef_core::ports::{EngineError, UiDispatcher};

    struct NoopUi;
    impl UiDispatcher for NoopUi {
        fn wake(&self) -> Result<(), EngineError> {
            Ok(())
        }
    }

    #[test]
    fn child_process_launch_strips_untrusted_switch_and_writes_authoritative() {
        let startup = StartupEndpoint::new(Arc::new(NoopUi));
        let schemes = vec![CustomSchemeConfig {
            scheme: "app".into(),
            options: CustomSchemeOptions::standard_app(),
            domain: None,
            asset_source: None,
        }];
        let encoded = encode_custom_schemes_switch(&schemes).unwrap();
        let app = RuntimeAppBuilder::new(
            startup,
            Weak::new(),
            Arc::new(AtomicU8::new(0)),
            Vec::new(),
            schemes,
            Some(encoded.clone()),
        );
        let Some(handler) = app.browser_process_handler() else {
            panic!("expected BrowserProcessHandler");
        };
        let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
        let Some(mut cmd) = command_line_create() else {
            panic!("command_line_create should succeed after api_hash");
        };
        cmd.append_switch_with_value(
            Some(&CefString::from(CUSTOM_SCHEMES_SWITCH)),
            Some(&CefString::from("evil:1")),
        );
        handler.on_before_child_process_launch(Some(&mut cmd));
        assert_eq!(
            CefString::from(&cmd.switch_value(Some(&CefString::from(CUSTOM_SCHEMES_SWITCH))))
                .to_string(),
            encoded
        );
    }

    #[test]
    fn before_command_line_processing_appends_offscreen_and_login_prompt_switches() {
        let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
        let startup = StartupEndpoint::new(Arc::new(NoopUi));
        let app = RuntimeAppBuilder::new(
            startup,
            Weak::new(),
            Arc::new(AtomicU8::new(0)),
            Vec::new(),
            Vec::new(),
            None,
        );
        let Some(mut cmd) = command_line_create() else {
            panic!("command_line_create should succeed after api_hash");
        };
        app.on_before_command_line_processing(None, Some(&mut cmd));
        assert_ne!(
            cmd.has_switch(Some(&CefString::from("no-startup-window"))),
            0
        );
        assert_ne!(
            cmd.has_switch(Some(&CefString::from("disable-chrome-login-prompt"))),
            0
        );
    }
}
