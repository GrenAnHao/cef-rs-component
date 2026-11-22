use rust_cef_runtime::subprocess::{SubprocessError, dispatch_subprocess};
use std::{cell::Cell, ffi::OsString};

fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

#[test]
fn no_type_or_empty_type_never_enters_cef() {
    for input in [args(&[]), args(&["--type="]), args(&["--type"])] {
        let called = Cell::new(false);
        let result = dispatch_subprocess(&input, || {
            called.set(true);
            Ok(0)
        });
        assert!(matches!(result, Err(SubprocessError::MissingProcessType)));
        assert!(!called.get());
    }
}

#[test]
fn recognized_entry_return_code_is_not_truncated() {
    for code in [0, 37, 0x1234, -2] {
        let called = Cell::new(0);
        let actual = dispatch_subprocess(&args(&["--type=renderer"]), || {
            called.set(called.get() + 1);
            Ok(code)
        })
        .unwrap();
        assert_eq!(actual, code);
        assert_eq!(called.get(), 1);
    }
}

#[test]
fn separate_type_argument_is_supported() {
    assert_eq!(
        dispatch_subprocess(&args(&["--type", "utility"]), || Ok(37)).unwrap(),
        37
    );
}

#[test]
fn cef_non_subprocess_sentinel_is_rejected() {
    let result = dispatch_subprocess(&args(&["--type=unknown"]), || Ok(-1));
    assert!(matches!(result, Err(SubprocessError::NotSubprocess)));
}
