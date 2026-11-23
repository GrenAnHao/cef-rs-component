use rust_cef_core::contracts::{
    BrowserConfig, BrowserCursor, BrowserId, BrowserOptions, BrowserSurfaceMode, CliSwitch,
    ConfigError, ConsolePolicy, ConsoleSeverity, CreateError, MAX_NATIVE_BOUND, MAX_NATIVE_COORD,
    MAX_SHORTCUT_RULES, NativeChildHwndConfig, PixelRect, ProfileHandle, ProfileStorage,
    RenderPreference, RuntimeConfig, RuntimeConfigBuilder, RuntimeFiles, RuntimeGeneration,
    RuntimeSettings, ShortcutFilter, ShortcutId, ShortcutRule, ThreadMode, TrustedContentPolicy,
    Viewport, progress_to_per_mille, truncate_utf8_bytes, zoom_level_to_milli,
};
use rust_cef_core::keymap::{EVENTFLAG_CONTROL_DOWN, EVENTFLAG_SHIFT_DOWN};
use rust_cef_core::ports::{BrowserCommand, BrowserEvent, EngineError};
use rust_cef_core::session::{Session, SessionState};

fn base_runtime_builder() -> RuntimeConfigBuilder {
    // These tests validate contracts, not filesystem/CEF deployment preflight.
    let directory = std::env::temp_dir().join("gpui-cef-contracts");
    RuntimeConfig::builder()
        .files(RuntimeFiles {
            directory: directory.clone(),
            helper_executable: directory.join("rust-cef-helper.exe"),
        })
        .root_cache_path(directory.join("cache"))
        .content_policy(TrustedContentPolicy {
            allowed_origins: Vec::new(),
            allow_embedded_data: true,
        })
}

fn browser_config(profile: Option<ProfileHandle>) -> BrowserConfig {
    BrowserConfig {
        initial_url: "data:text/html,contract-test".to_string(),
        profile,
        render_preference: RenderPreference::SoftwareRequired,
        viewport: Viewport {
            width: 640,
            height: 480,
            scale: 1.0,
        },
        options: Default::default(),
    }
}

#[test]
fn foreign_generation_rejected() {
    let browser = BrowserId::new(RuntimeGeneration::new(1), 1);
    let foreign_profile = ProfileHandle::new(RuntimeGeneration::new(2), 1);

    let result = Session::begin_create(browser, browser_config(Some(foreign_profile)));

    assert!(matches!(result, Err(CreateError::WrongRuntime)));
}

#[test]
fn thread_mode_required() {
    let result = base_runtime_builder()
        .default_profile(ProfileStorage::Memory)
        .build();

    assert!(matches!(result, Err(ConfigError::MissingThreadMode)));
}

#[test]
fn storage_policy_required() {
    let result = base_runtime_builder()
        .thread_mode(ThreadMode::Integrated)
        .build();

    assert!(matches!(result, Err(ConfigError::MissingDefaultProfile)));
}

#[test]
fn each_explicit_mode_and_storage_is_preserved() {
    for mode in [ThreadMode::Integrated, ThreadMode::Threaded] {
        for storage in [
            ProfileStorage::Memory,
            ProfileStorage::Persistent {
                directory: std::env::temp_dir().join("profile"),
            },
        ] {
            let config = base_runtime_builder()
                .thread_mode(mode)
                .default_profile(storage.clone())
                .build()
                .unwrap();
            assert_eq!(config.thread_mode, mode);
            assert_eq!(config.default_profile, storage);
            assert!(config.content_policy.allow_embedded_data);
        }
    }
}

#[test]
fn all_deployment_inputs_are_required() {
    let builder = RuntimeConfig::builder()
        .thread_mode(ThreadMode::Threaded)
        .default_profile(ProfileStorage::Memory);
    assert!(matches!(
        builder.clone().build(),
        Err(ConfigError::MissingRuntimeFiles)
    ));
    let directory = std::env::temp_dir();
    let builder = builder.files(RuntimeFiles {
        directory: directory.clone(),
        helper_executable: directory.join("rust-cef-helper.exe"),
    });
    assert!(matches!(
        builder.clone().build(),
        Err(ConfigError::MissingRootCachePath)
    ));
    let builder = builder.root_cache_path(directory);
    assert!(matches!(
        builder.build(),
        Err(ConfigError::MissingContentPolicy)
    ));
}

#[test]
fn same_generation_profile_can_begin_creation() {
    let generation = RuntimeGeneration::new(5);
    let profile = ProfileHandle::new(generation, 2);
    let session =
        Session::begin_create(BrowserId::new(generation, 3), browser_config(Some(profile)))
            .unwrap();
    assert_eq!(session.snapshot().state, SessionState::Creating);
    assert!(!session.snapshot().creation_accepted);
}

#[test]
fn create_accept_is_not_ready() {
    let browser = BrowserId::new(RuntimeGeneration::new(1), 1);
    let mut session = Session::begin_create(browser, browser_config(None)).unwrap();
    assert!(!session.snapshot().creation_accepted);

    session.accept_create_request();

    let snapshot = session.snapshot();
    assert!(snapshot.creation_accepted);
    assert_eq!(snapshot.state, SessionState::Creating);
    assert!(!snapshot.is_ready());
}

#[test]
fn runtime_settings_rejects_forbidden_and_accepts_allowlisted_switches() {
    for forbidden in [
        "single-process",
        "user-data-dir",
        "cache-path",
        "remote-debugging-port",
        "--lang",
        "unknown-arbitrary-switch",
    ] {
        let err = base_runtime_builder()
            .thread_mode(ThreadMode::Threaded)
            .default_profile(ProfileStorage::Memory)
            .allowlisted_switch(forbidden, None)
            .build()
            .unwrap_err();
        assert_eq!(err, ConfigError::ForbiddenSwitch, "switch={forbidden}");
    }

    let ok = base_runtime_builder()
        .thread_mode(ThreadMode::Threaded)
        .default_profile(ProfileStorage::Memory)
        .user_agent("CustomAgent/1.0")
        .locale("zh-CN")
        .accept_language_list("zh-CN,en-US")
        .background_color(0xFF102030)
        .allowlisted_switch("mute-audio", None)
        .allowlisted_switch("autoplay-policy", Some("no-user-gesture-required".into()))
        .build()
        .unwrap();
    assert_eq!(ok.settings.user_agent.as_deref(), Some("CustomAgent/1.0"));
    assert_eq!(ok.settings.locale.as_deref(), Some("zh-CN"));
    assert_eq!(
        ok.settings.accept_language_list.as_deref(),
        Some("zh-CN,en-US")
    );
    assert_eq!(ok.settings.background_color, Some(0xFF102030));
    assert_eq!(
        ok.settings.allowlisted_switches,
        vec![
            CliSwitch::flag("mute-audio"),
            CliSwitch::with_value("autoplay-policy", "no-user-gesture-required"),
        ]
    );
}

#[test]
fn non_finite_and_out_of_range_zoom_rejected_before_queueing() {
    let browser = BrowserId::new(RuntimeGeneration::new(1), 1);
    let mut session = Session::begin_create(browser, browser_config(None)).unwrap();
    session.after_created();

    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -9.5, 9.5] {
        assert_eq!(zoom_level_to_milli(bad), Err(EngineError::InvalidRequest));
        assert_eq!(
            session.admit_command(BrowserCommand::SetZoomLevel(bad)),
            Err(EngineError::InvalidRequest)
        );
    }
    assert_eq!(session.pending_commands(), 0);
    assert!(
        session
            .admit_command(BrowserCommand::SetZoomLevel(1.25))
            .is_ok()
    );
    assert_eq!(session.pending_commands(), 1);
}

#[test]
fn utf8_truncation_and_progress_clamping_keep_boundaries_valid() {
    let text = "abc你好世界xyz";
    for limit in 0..=text.len() + 2 {
        let clipped = truncate_utf8_bytes(text, limit);
        assert!(clipped.len() <= limit);
        assert!(text.starts_with(&clipped));
    }
    assert_eq!(truncate_utf8_bytes("你好", 4), "你");
    assert_eq!(progress_to_per_mille(f64::NAN), 0);
    assert_eq!(progress_to_per_mille(-0.2), 0);
    assert_eq!(progress_to_per_mille(0.5554), 555);
    assert_eq!(progress_to_per_mille(1.8), 1000);
}

#[test]
fn shortcut_filter_enforces_cap_and_modifier_masking() {
    let too_many = (1..=(MAX_SHORTCUT_RULES as u32 + 1))
        .map(|i| ShortcutRule {
            id: ShortcutId(i),
            windows_key_code: 0x41,
            shift: false,
            control: true,
            alt: false,
            platform: false,
        })
        .collect();
    assert_eq!(
        ShortcutFilter::new(too_many),
        Err(ConfigError::InvalidShortcutFilter)
    );

    let filter = ShortcutFilter::new(vec![
        ShortcutRule {
            id: ShortcutId(10),
            windows_key_code: 0x4D,
            shift: false,
            control: true,
            alt: false,
            platform: false,
        },
        ShortcutRule {
            id: ShortcutId(11),
            windows_key_code: 0x4D,
            shift: true,
            control: true,
            alt: false,
            platform: false,
        },
    ])
    .unwrap();
    assert_eq!(
        filter.match_key(0x4D, EVENTFLAG_CONTROL_DOWN),
        Some(ShortcutId(10))
    );
    assert_eq!(
        filter.match_key(0x4D, EVENTFLAG_CONTROL_DOWN | EVENTFLAG_SHIFT_DOWN),
        Some(ShortcutId(11))
    );
    assert_eq!(filter.match_key(0x4D, 0), None);
}

#[test]
fn browser_options_validate_frame_rate_encoding_and_console_bounds() {
    let browser = BrowserId::new(RuntimeGeneration::new(1), 1);
    for invalid in [
        BrowserOptions::default().with_frame_rate(0),
        BrowserOptions::default().with_frame_rate(241),
        BrowserOptions::default().with_default_encoding("  "),
        BrowserOptions::default().with_console_policy(ConsolePolicy {
            enabled: true,
            max_message_bytes: 0,
        }),
        BrowserOptions::default().with_console_policy(ConsolePolicy {
            enabled: true,
            max_message_bytes: 8192,
        }),
    ] {
        let mut cfg = browser_config(None);
        cfg.options = invalid;
        assert!(matches!(
            Session::begin_create(browser, cfg),
            Err(CreateError::InvalidOptions)
        ));
    }

    let defaults = BrowserOptions::default();
    assert_eq!(defaults.accelerated_osr, cfg!(target_os = "windows"));
    assert_eq!(
        defaults.effective_frame_rate(),
        rust_cef_core::contracts::DEFAULT_WINDOWLESS_FRAME_RATE
    );
    assert_eq!(defaults.effective_frame_rate(), 120);
    assert!(!defaults.with_accelerated_osr(false).accelerated_osr);

    let mut cfg = browser_config(None);
    cfg.options = BrowserOptions::default()
        .with_frame_rate(120)
        .with_default_encoding("UTF-8")
        .with_tooltip_events(true);
    let mut session = Session::begin_create(browser, cfg).unwrap();
    assert!(
        session.apply_navigation_event(&BrowserEvent::CursorChanged(BrowserCursor::PointingHand))
    );
    assert!(session.apply_navigation_event(&BrowserEvent::TooltipChanged("hi".into())));
    assert!(session.apply_navigation_event(&BrowserEvent::ZoomLevelChanged(1500)));
    assert!(session.apply_navigation_event(&BrowserEvent::AudioMutedChanged(true)));
    let snap = session.snapshot();
    assert_eq!(snap.navigation.cursor, BrowserCursor::PointingHand);
    assert_eq!(snap.navigation.tooltip, "hi");
    assert!((snap.navigation.zoom_level() - 1.5).abs() < 1e-9);
    assert!(snap.navigation.audio_muted);

    let redacted_dbg = format!(
        "{:?}",
        BrowserEvent::ConsoleMessage {
            severity: ConsoleSeverity::Warning,
            message: "secret-console-payload".into(),
            line: 42,
        }
    );
    assert!(!redacted_dbg.contains("secret-console-payload"));
    assert!(redacted_dbg.contains("message_bytes: 22"));

    let exec_cmd = BrowserCommand::ExecuteJavaScript {
        script: "SECRET_INLINE_JS()".into(),
        frame_scope: rust_cef_core::js_bridge::JsFrameScope::MainFrame,
    };
    let exec_dbg = format!("{exec_cmd:?}");
    assert!(!exec_dbg.contains("SECRET_INLINE_JS"));
    assert!(exec_dbg.contains("script_bytes: 18"));

    let post_evt = BrowserEvent::JsMessageReceived {
        origin: "app://local".into(),
        payload_json: r#"{"token":"SECRET_TOKEN_XYZ"}"#.into(),
    };
    let post_dbg = format!("{post_evt:?}");
    assert!(!post_dbg.contains("SECRET_TOKEN_XYZ"));
    assert!(post_dbg.contains("payload_bytes:"));
}

#[test]
fn runtime_config_rejects_reserved_or_forbidden_custom_scheme_switch() {
    use rust_cef_core::custom_schemes::{
        CustomSchemeConfig, CustomSchemeOptions, CustomSchemeValidationError,
    };
    let browser = BrowserId::new(RuntimeGeneration::new(1), 1);
    let err = base_runtime_builder()
        .thread_mode(ThreadMode::Integrated)
        .default_profile(ProfileStorage::Memory)
        .custom_scheme(CustomSchemeConfig {
            scheme: "https".into(),
            options: CustomSchemeOptions::standard_app(),
            domain: None,
            asset_source: None,
        })
        .build()
        .unwrap_err();
    assert_eq!(
        err,
        ConfigError::InvalidCustomSchemes(CustomSchemeValidationError::ReservedScheme)
    );

    let err_switch = base_runtime_builder()
        .thread_mode(ThreadMode::Integrated)
        .default_profile(ProfileStorage::Memory)
        .allowlisted_switch("rust-cef-custom-schemes", Some("app:1".into()))
        .build()
        .unwrap_err();
    assert_eq!(err_switch, ConfigError::ForbiddenSwitch);

    let mut cfg = browser_config(None);
    let mut sess = Session::begin_create(browser, cfg.clone()).unwrap();
    sess.after_created();
    assert_eq!(
        sess.admit_command(BrowserCommand::PostJsMessage {
            payload_json: "{}".into()
        }),
        Err(EngineError::PolicyDenied)
    );
    cfg.options =
        BrowserOptions::default().with_js_bridge(rust_cef_core::js_bridge::JsBridgeConfig {
            enabled: true,
            allowed_origins: vec!["app://local".into()],
            ..Default::default()
        });
    let mut sess2 = Session::begin_create(browser, cfg).unwrap();
    sess2.after_created();
    assert!(
        sess2
            .admit_command(BrowserCommand::PostJsMessage {
                payload_json: r#"{"ok":true}"#.into()
            })
            .is_ok()
    );
}

#[test]
fn surface_mode_pixel_rect_and_runtime_windowless_contracts() {
    use std::num::NonZeroUsize;

    assert_eq!(BrowserSurfaceMode::default(), BrowserSurfaceMode::Offscreen);
    assert!(RuntimeSettings::default().windowless_rendering_enabled);

    let rt_disabled = base_runtime_builder()
        .thread_mode(ThreadMode::Threaded)
        .default_profile(ProfileStorage::Memory)
        .windowless_rendering_enabled(false)
        .build()
        .unwrap();
    assert!(!rt_disabled.settings.windowless_rendering_enabled);

    assert_eq!(
        PixelRect::try_from_i32(0, 0, -1, 100),
        Err(EngineError::InvalidRequest)
    );
    assert_eq!(
        PixelRect::try_from_i32(0, 0, 100, -1),
        Err(EngineError::InvalidRequest)
    );
    assert_eq!(
        PixelRect::try_from_i32(0, 0, MAX_NATIVE_BOUND as i32 + 1, 100),
        Err(EngineError::InvalidRequest)
    );
    assert_eq!(
        PixelRect::try_from_i32(MAX_NATIVE_COORD + 1, 0, 100, 100),
        Err(EngineError::InvalidRequest)
    );
    assert_eq!(
        PixelRect::try_from_i32(-(MAX_NATIVE_COORD + 1), 0, 100, 100),
        Err(EngineError::InvalidRequest)
    );
    let zero_rect = PixelRect::try_from_i32(10, 20, 0, 0).unwrap();
    assert!(zero_rect.is_empty());

    let parent = NonZeroUsize::new(0x1234_5678).unwrap();
    let cfg = NativeChildHwndConfig {
        parent_hwnd: parent,
        initial_bounds: PixelRect {
            x: 10,
            y: 20,
            width: 800,
            height: 600,
        },
        activate_on_creation: false,
    };
    let dbg = format!("{cfg:?}");
    assert!(dbg.contains("0x12345678"));

    let bad_opts = BrowserOptions::default().with_surface_mode(
        BrowserSurfaceMode::NativeChildHwnd(NativeChildHwndConfig {
            parent_hwnd: parent,
            initial_bounds: PixelRect {
                x: 0,
                y: 0,
                width: MAX_NATIVE_BOUND + 1,
                height: 100,
            },
            activate_on_creation: false,
        }),
    );
    assert_eq!(bad_opts.validate(), Err(CreateError::InvalidOptions));

    assert_eq!(
        BrowserSurfaceMode::Offscreen.validate_for_runtime(false),
        Err(CreateError::InvalidOptions)
    );
    assert_eq!(
        BrowserSurfaceMode::NativeChildHwnd(cfg).validate_for_runtime(false),
        Ok(())
    );
}
