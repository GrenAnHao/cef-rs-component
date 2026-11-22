// Shared compile-time cohort metadata. The host validator embeds only its digest.
pub const COMPONENT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const TARGET: &str = "x86_64-pc-windows-msvc";
pub const CEF_VERSION: &str = "156.0.31";
pub const BINDING_REVISION: &str = "7b6bf84e76e919790bd61df077e29dc4132eb5e2";
pub const HELPER_PREFIX: &[u8] = b"GPUI-CEF-COMPONENT|helper|schema=1|";
pub const HELPER_STAMP: &str = concat!(
    "GPUI-CEF-COMPONENT|helper|schema=1|target=x86_64-pc-windows-msvc|component=",
    env!("CARGO_PKG_VERSION"),
    "|cef=156.0.31|binding=7b6bf84e76e919790bd61df077e29dc4132eb5e2"
);
