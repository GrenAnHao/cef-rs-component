use rust_cef_gpui::contracts::{RenderPreference, ThreadMode};
use std::{collections::BTreeMap, ffi::OsString, path::PathBuf, time::Duration};

pub(super) const USAGE: &str = "gpui-cef-component-demo --bundle <absolute-directory> --helper <absolute-exe> --cache-root <absolute-directory> --mode Integrated|Threaded --backend Software|D3d11 [--host-mode osr|native-hwnd] [--smoke-ms 100..30000 [--observer-gate <absolute-file.release>]] [--context-menu true|false [--menu-structure true|false] [--menu-devtools true|false]] [--ux-controls true|false] [--host-capabilities true|false] [--browser-shell true|false] [--cef-example true|false] [--page demo|menu|js-bridge] [--url http://...|https://...]";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum HostMode {
    #[default]
    Osr,
    NativeHwnd,
}

pub(super) struct Config {
    pub bundle: PathBuf,
    pub helper: PathBuf,
    pub cache_root: PathBuf,
    pub mode: ThreadMode,
    pub backend: RenderPreference,
    pub host_mode: HostMode,
    pub smoke: Option<Duration>,
    pub observer_gate: Option<PathBuf>,
    /// Opt-in: attach the real default context-menu services to the delivered
    /// view. Off by default so the packaged smoke path stays passive.
    pub context_menu: bool,
    /// Host-managed menu capabilities, each an explicit opt-in. CEF's windowless
    /// model carries edit commands only, so these items exist only when the host
    /// asks for them.
    pub menu_structure: bool,
    pub menu_devtools: bool,
    /// Absolute path for the delivered host's menu receipt, written each UI turn
    /// so an external driver can read the rows a real click would select.
    pub menu_receipt: Option<PathBuf>,
    /// Which page the delivered host loads. `demo` is the offline showcase page;
    /// `menu` is the editable fixture the menu probes need; `js-bridge` demonstrates
    /// `app://demo` custom scheme + origin-gated `window.rustCef` messages.
    pub menu_page: bool,
    pub js_bridge_page: bool,
    /// Opt-in compact toolbar showcasing cursor, tooltip, status, zoom, mute,
    /// and pre-key shortcut intercept. Off by default for passive smoke runs.
    pub ux_controls: bool,
    /// Opt-in host capability services (controlled popups, confined downloads,
    /// JS dialogs, and headless CDP). Off by default to keep default-deny.
    pub host_capabilities: bool,
    /// Opt-in desktop multi-tab browser shell mode built strictly over public
    /// `rust-cef-gpui` and `rust-cef-core` APIs.
    pub browser_shell: bool,
    /// Opt-in desktop browser example showcase mode.
    pub cef_example: bool,
    /// Optional explicit initial `http://` or `https://` URL for interactive testing.
    pub custom_url: Option<String>,
}

pub(super) fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Option<Config>, String> {
    let mut args = args.into_iter().peekable();
    if args.peek().is_some_and(|arg| arg == "--help") {
        args.next();
        return if args.next().is_none() {
            Ok(None)
        } else {
            Err("--help must be used alone".into())
        };
    }
    let mut values = BTreeMap::new();
    while let Some(name) = args.next() {
        let name = match name.to_str() {
            Some(
                n @ ("--bundle"
                | "--helper"
                | "--cache-root"
                | "--mode"
                | "--backend"
                | "--host-mode"
                | "--smoke-ms"
                | "--observer-gate"
                | "--context-menu"
                | "--menu-structure"
                | "--menu-devtools"
                | "--menu-receipt"
                | "--page"
                | "--ux-controls"
                | "--host-capabilities"
                | "--browser-shell"
                | "--cef-example"
                | "--url"),
            ) => n.to_owned(),
            _ => return Err("unknown component demo argument".into()),
        };
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {name}"))?;
        if values.insert(name.clone(), value).is_some() {
            return Err(format!("duplicate option {name}"));
        }
    }
    let mut required = |name: &str| values.remove(name).ok_or_else(|| format!("missing {name}"));
    let bundle = absolute("--bundle", required("--bundle")?)?;
    let helper = absolute("--helper", required("--helper")?)?;
    let cache_root = absolute("--cache-root", required("--cache-root")?)?;
    let mode = match required("--mode")?.to_str() {
        Some("Integrated") => ThreadMode::Integrated,
        Some("Threaded") => ThreadMode::Threaded,
        _ => return Err("--mode must name Integrated or Threaded".into()),
    };
    let backend = match required("--backend")?.to_str() {
        Some("Software") => RenderPreference::SoftwareRequired,
        Some("D3d11") => RenderPreference::D3d11Required,
        _ => return Err("--backend must name Software or D3d11 (Required)".into()),
    };
    let host_mode = match values
        .remove("--host-mode")
        .as_ref()
        .and_then(|v| v.to_str())
    {
        None | Some("osr") => HostMode::Osr,
        Some("native-hwnd") => HostMode::NativeHwnd,
        Some(_) => return Err("--host-mode must be osr or native-hwnd".into()),
    };
    let smoke = values
        .remove("--smoke-ms")
        .map(|v| {
            v.to_str()
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|ms| (100..=30000).contains(ms))
                .map(Duration::from_millis)
                .ok_or("--smoke-ms must be an integer in 100..30000")
        })
        .transpose()?;
    let observer_gate = values
        .remove("--observer-gate")
        .map(|v| absolute("--observer-gate", v))
        .transpose()?;
    if let Some(gate) = &observer_gate {
        if smoke.is_none() {
            return Err("--observer-gate requires --smoke-ms".into());
        }
        if !gate.extension().is_some_and(|ext| ext == "release") {
            return Err("--observer-gate must end in .release".into());
        }
    }
    // Boolean opt-in: the flag takes the literal value `true` so every option
    // keeps the same name/value shape.
    let context_menu = match values.remove("--context-menu") {
        None => false,
        Some(v) if v == "true" => true,
        Some(v) if v == "false" => false,
        Some(_) => return Err("--context-menu must be true or false".into()),
    };
    let mut flag = |name: &str| match values.remove(name) {
        None => Ok(false),
        Some(v) if v == "true" => Ok(true),
        Some(v) if v == "false" => Ok(false),
        Some(_) => Err(format!("{name} must be true or false")),
    };
    let menu_structure = flag("--menu-structure")?;
    let menu_devtools = flag("--menu-devtools")?;
    let ux_controls = flag("--ux-controls")?;
    let host_capabilities = flag("--host-capabilities")?;
    let browser_shell = flag("--browser-shell")?;
    let cef_example = flag("--cef-example")?;
    if (menu_structure || menu_devtools) && !context_menu {
        return Err("menu capabilities require --context-menu true".into());
    }
    let raw_page = values.remove("--page");
    let (menu_page, js_bridge_page) = match raw_page.as_ref() {
        None => (false, false),
        Some(v) if v == "demo" => (false, false),
        Some(v) if v == "menu" => (true, false),
        Some(v) if v == "js-bridge" => (false, true),
        Some(_) => return Err("--page must be demo, menu, or js-bridge".into()),
    };
    let custom_url = match values.remove("--url") {
        None => None,
        Some(v) => {
            if raw_page.is_some() {
                return Err("--url cannot be combined with --page".into());
            }
            let url = v
                .into_string()
                .map_err(|_| "--url must be valid UTF-8".to_owned())?;
            if !(url.starts_with("http://") || url.starts_with("https://"))
                || url.chars().any(|c| c.is_control())
            {
                return Err(
                    "--url must start with http:// or https:// without control characters".into(),
                );
            }
            Some(url)
        }
    };
    let menu_receipt = match values.remove("--menu-receipt") {
        None => None,
        Some(v) => {
            if !context_menu {
                return Err("--menu-receipt requires --context-menu true".into());
            }
            Some(absolute("--menu-receipt", v)?)
        }
    };
    if !values.is_empty() {
        return Err("unknown component demo argument".into());
    }
    Ok(Some(Config {
        bundle,
        helper,
        cache_root,
        mode,
        backend,
        host_mode,
        smoke,
        observer_gate,
        context_menu,
        menu_structure,
        menu_devtools,
        menu_receipt,
        menu_page,
        js_bridge_page,
        ux_controls,
        host_capabilities,
        browser_shell,
        cef_example,
        custom_url,
    }))
}

fn absolute(name: &str, value: OsString) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(format!("{name} must be an explicit absolute path"));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> Vec<OsString> {
        [
            "--bundle",
            r"D:\发行 with spaces",
            "--helper",
            r"D:\发行 with spaces\worker.exe",
            "--cache-root",
            r"D:\运行目录\cache",
            "--mode",
            "Threaded",
            "--backend",
            "Software",
        ]
        .into_iter()
        .map(OsString::from)
        .collect()
    }

    #[test]
    fn preserves_explicit_windows_paths_and_required_backend() {
        let c = parse(args()).unwrap().unwrap();
        assert_eq!(c.bundle, PathBuf::from(r"D:\发行 with spaces"));
        assert_eq!(c.helper, PathBuf::from(r"D:\发行 with spaces\worker.exe"));
        assert_eq!(c.mode, ThreadMode::Threaded);
        assert_eq!(c.backend, RenderPreference::SoftwareRequired);
        assert!(c.smoke.is_none());
    }

    #[test]
    fn rejects_implicit_relative_or_duplicate_choices() {
        assert!(parse(Vec::new()).is_err());
        let mut v = args();
        v[3] = r"D:worker.exe".into();
        assert!(parse(v).is_err());
        let mut v = args();
        v.extend(["--mode".into(), "Integrated".into()]);
        assert!(parse(v).is_err());
        let mut v = args();
        v[9] = "Auto".into();
        assert!(parse(v).is_err());
    }

    #[test]
    fn requires_explicit_bounded_smoke_for_observer() {
        let mut v = args();
        v.extend(["--observer-gate".into(), r"D:\case\observer.release".into()]);
        assert!(parse(v.clone()).is_err());
        v.extend(["--smoke-ms".into(), "100".into()]);
        let c = parse(v.clone()).unwrap().unwrap();
        assert_eq!(c.smoke, Some(Duration::from_millis(100)));
        *v.last_mut().unwrap() = "30001".into();
        assert!(parse(v).is_err());
    }

    #[test]
    fn help_does_not_accept_a_second_argument() {
        assert!(parse(["--help".into()]).unwrap().is_none());
        assert!(parse(["--help".into(), "--mode".into()]).is_err());
    }

    #[test]
    fn ux_controls_defaults_false_and_validates_boolean() {
        assert!(!parse(args()).unwrap().unwrap().ux_controls);
        assert!(!parse(args()).unwrap().unwrap().host_capabilities);
        assert!(!parse(args()).unwrap().unwrap().js_bridge_page);
        let mut enabled = args();
        enabled.extend([
            "--ux-controls".into(),
            "true".into(),
            "--host-capabilities".into(),
            "true".into(),
        ]);
        let parsed_enabled = parse(enabled).unwrap().unwrap();
        assert!(parsed_enabled.ux_controls);
        assert!(parsed_enabled.host_capabilities);
        let mut invalid = args();
        invalid.extend(["--ux-controls".into(), "yes".into()]);
        assert!(parse(invalid).is_err());
        let mut invalid_caps = args();
        invalid_caps.extend(["--host-capabilities".into(), "maybe".into()]);
        assert!(parse(invalid_caps).is_err());
        let mut js_page = args();
        js_page.extend(["--page".into(), "js-bridge".into()]);
        let c = parse(js_page).unwrap().unwrap();
        assert!(c.js_bridge_page);
        assert!(!c.menu_page);
        assert!(!c.browser_shell);
    }

    #[test]
    fn browser_shell_defaults_false_validates_boolean_and_coexists() {
        assert!(!parse(args()).unwrap().unwrap().browser_shell);

        let mut on = args();
        on.extend([
            "--smoke-ms".into(),
            "500".into(),
            "--context-menu".into(),
            "true".into(),
            "--menu-structure".into(),
            "true".into(),
            "--menu-devtools".into(),
            "true".into(),
            "--ux-controls".into(),
            "true".into(),
            "--host-capabilities".into(),
            "true".into(),
            "--browser-shell".into(),
            "true".into(),
            "--page".into(),
            "demo".into(),
        ]);
        let parsed = parse(on).unwrap().unwrap();
        assert!(parsed.browser_shell);
        assert!(parsed.context_menu);
        assert!(parsed.ux_controls);
        assert!(parsed.host_capabilities);

        let mut off = args();
        off.extend(["--browser-shell".into(), "false".into()]);
        assert!(!parse(off).unwrap().unwrap().browser_shell);

        let mut invalid = args();
        invalid.extend(["--browser-shell".into(), "maybe".into()]);
        assert!(parse(invalid).is_err());

        let mut example = args();
        example.extend(["--cef-example".into(), "true".into()]);
        assert!(parse(example).unwrap().unwrap().cef_example);
        assert!(!parse(args()).unwrap().unwrap().cef_example);
    }

    #[test]
    fn host_mode_defaults_osr_and_validates_explicit_values() {
        assert_eq!(parse(args()).unwrap().unwrap().host_mode, HostMode::Osr);

        let mut osr = args();
        osr.extend(["--host-mode".into(), "osr".into()]);
        assert_eq!(parse(osr).unwrap().unwrap().host_mode, HostMode::Osr);

        let mut native = args();
        native.extend(["--host-mode".into(), "native-hwnd".into()]);
        assert_eq!(
            parse(native).unwrap().unwrap().host_mode,
            HostMode::NativeHwnd
        );

        let mut invalid = args();
        invalid.extend(["--host-mode".into(), "chrome-views".into()]);
        assert!(parse(invalid).is_err());
    }

    #[test]
    fn custom_url_validates_http_https_and_conflicts_with_page() {
        assert_eq!(parse(args()).unwrap().unwrap().custom_url, None);

        let mut ok = args();
        ok.extend(["--url".into(), "https://cn.bing.com/search?q=gpui".into()]);
        assert_eq!(
            parse(ok).unwrap().unwrap().custom_url.as_deref(),
            Some("https://cn.bing.com/search?q=gpui")
        );

        let mut bad_scheme = args();
        bad_scheme.extend(["--url".into(), r"file:///C:/a.html".into()]);
        assert!(parse(bad_scheme).is_err());

        let mut bad_ctrl = args();
        bad_ctrl.extend(["--url".into(), "https://cn.bing.com/\n".into()]);
        assert!(parse(bad_ctrl).is_err());

        let mut conflict = args();
        conflict.extend([
            "--url".into(),
            "https://cn.bing.com/search?q=gpui".into(),
            "--page".into(),
            "demo".into(),
        ]);
        assert!(parse(conflict).is_err());
    }
}
