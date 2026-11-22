use cef::*;

wrap_app! {
    pub(crate) struct CefAppBuilder;

    impl App {
        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&CefStringUtf16>,
            command_line: Option<&mut CommandLine>,
        ) {
            if let Some(command_line) = command_line {
                command_line.append_switch(Some(&"no-startup-window".into()));
                command_line.append_switch_with_value(
                    Some(&"enable-logging".into()),
                    Some(&"stderr".into()),
                );
            }
        }
    }
}
