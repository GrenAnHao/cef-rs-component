//! Platform-independent contracts and pure state for GPUI-CEF-COMPONENT.
//! Native runtime, capture and GPUI operations are injected through ports.

pub mod accessibility;
pub mod contracts;
pub mod custom_schemes;
pub mod file_drop;
pub mod find;
pub mod frame_budget;
pub mod frame_layout;
pub mod frame_store;
pub mod host_capabilities;
pub mod ime;
pub mod input;
pub mod js_bridge;
pub mod keymap;
pub mod mailbox;
pub mod menu;
pub mod menu_context;
pub mod menu_lease;
pub mod menu_model;
pub mod menu_requests;
pub mod outbound_drag;
pub mod ports;
pub mod print;
pub mod profiles;
pub mod resize_recovery;
pub mod runtime;
pub mod session;
pub mod source_view;
pub mod url_request;
