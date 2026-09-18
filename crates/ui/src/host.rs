//! Bridge to the C++ host in `cpp/host.cpp`.

#[cxx::bridge]
mod ffi {
    unsafe extern "C++" {
        include!("parla-ui/cpp/host.h");
        /// Creates the application, loads the QML and runs the event loop.
        fn run_ui(start_hidden: bool) -> i32;
    }
}

/// Run the UI until the application quits; returns the exit code.
pub fn run(start_hidden: bool) -> i32 {
    // The static QML module with the Rust QObjects must be linked in.
    cxx_qt::init_qml_module!("org.parla.ui");
    ffi::run_ui(start_hidden)
}
