fn main() {
    match rust_cef_runtime::subprocess::execute_current_subprocess() {
        // Preserve CEF's complete platform exit code, not an eight-bit truncation.
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!(
                "{}: {error}",
                rust_cef_runtime::subprocess::helper_build_stamp()
            );
            std::process::exit(2);
        }
    }
}
