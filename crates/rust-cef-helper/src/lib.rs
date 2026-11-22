//! The independent helper executable delegates only CEF subprocess execution.
//! No GPUI dependency, Application startup, or browser-process initialization.

#[cfg(test)]
mod tests {
    #[test]
    fn windows_manifest_declares_win10_compatibility_and_per_monitor_v2() {
        let manifest = include_str!("../app.manifest");
        assert!(
            manifest.contains("{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"),
            "helper manifest must declare Windows 10/11 compatibility (CefSharp.BrowserSubprocess/app.manifest)"
        );
        assert!(
            manifest.contains("PerMonitorV2"),
            "helper manifest must declare PerMonitorV2 DPI awareness"
        );
    }
}
