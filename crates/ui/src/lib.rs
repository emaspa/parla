//! parla-ui: the desktop face of parla. The Rust side owns the D-Bus client
//! ([`daemon`]) and the editable data files ([`store`]); the window itself
//! is QML hosted by a small C++ shim (`cpp/host.cpp`, bridged in [`host`])
//! because Kirigami's desktop style and the tray item want a QApplication.
//!
//! This is a library only so that both binaries (`parla-ui` and the mock
//! daemon `parla-mockd`) link the same generated Qt code.

pub mod daemon;
pub mod host;
pub mod store;
