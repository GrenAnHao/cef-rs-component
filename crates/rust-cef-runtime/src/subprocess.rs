use std::ffi::OsString;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubprocessError {
    MissingProcessType,
    NotSubprocess,
    UnsupportedPlatform,
    CefVersionMismatch { actual: [i32; 3] },
    ApiHashUnavailable,
}

impl std::fmt::Display for SubprocessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingProcessType => write!(f, "a nonempty CEF --type is required"),
            Self::NotSubprocess => write!(f, "CEF did not recognize this as a subprocess"),
            Self::UnsupportedPlatform => write!(f, "helper supports Windows x64 MSVC only"),
            Self::CefVersionMismatch { actual } => {
                write!(f, "loaded CEF {actual:?}, expected [156, 0, 31]")
            }
            Self::ApiHashUnavailable => write!(f, "CEF API hash selection failed"),
        }
    }
}

impl std::error::Error for SubprocessError {}

/// A testable entry boundary, not a browser-process initializer.
/// Only CEF decides whether a nonempty candidate process type is recognized.
pub fn dispatch_subprocess(
    arguments: &[OsString],
    execute: impl FnOnce() -> Result<i32, SubprocessError>,
) -> Result<i32, SubprocessError> {
    let mut process_type = None;
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        if let Some(value) = argument.to_str().and_then(|s| s.strip_prefix("--type=")) {
            process_type = Some(value.to_string());
        } else if argument == "--type" {
            process_type = Some(
                arguments
                    .next()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string(),
            );
        }
    }
    if process_type.as_ref().is_none_or(|s| s.trim().is_empty()) {
        return Err(SubprocessError::MissingProcessType);
    }
    match execute()? {
        -1 => Err(SubprocessError::NotSubprocess),
        code => Ok(code),
    }
}

pub fn helper_build_stamp() -> &'static str {
    crate::build_stamp::HELPER_STAMP
}

pub fn execute_current_subprocess() -> Result<i32, SubprocessError> {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    dispatch_subprocess(&arguments, execute_native)
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
unsafe extern "C" {
    // Exact C int(int) signature: CEF156 include/cef_version_info.h:47-60.
    // The fixed Rust bindings omit it; libcef.dll's export was checked directly.
    fn cef_version_info(entry: i32) -> i32;
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn loaded_cef_version() -> [i32; 3] {
    // SAFETY: bootstrap function has no pointer arguments and returns static
    // library version data. libcef is linked through the fixed cef-rs dependency.
    unsafe {
        [
            cef_version_info(0),
            cef_version_info(1),
            cef_version_info(2),
        ]
    }
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub(crate) fn check_loaded_cef() -> Result<(), SubprocessError> {
    let actual = loaded_cef_version();
    if actual != [156, 0, 31] {
        return Err(SubprocessError::CefVersionMismatch { actual });
    }
    Ok(())
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn execute_native() -> Result<i32, SubprocessError> {
    check_loaded_cef()?;
    let args = cef::args::Args::new();
    if cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0).is_null() {
        return Err(SubprocessError::ApiHashUnavailable);
    }
    let raw_args: Vec<OsString> = std::env::args_os().collect();
    let mut app = crate::helper_app::build_helper_app(&raw_args);
    Ok(cef::execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    ))
}

#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
fn execute_native() -> Result<i32, SubprocessError> {
    Err(SubprocessError::UnsupportedPlatform)
}

#[cfg(all(test, target_os = "windows", target_arch = "x86_64"))]
mod tests {
    #[test]
    fn actual_loaded_bootstrap_version_matches_locked_sdk() {
        assert_eq!(super::loaded_cef_version(), [156, 0, 31]);
        super::check_loaded_cef().unwrap();
    }
}
