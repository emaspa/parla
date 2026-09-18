//! desktopd: the executor for parla. One implementation of the desktop tool
//! surface (plan §1), exposed as a Rust library for the daemon's fast path
//! and — in P3 — as an MCP server for the agent path.

pub mod a11y;
pub mod bus;
pub mod command;
pub mod config;
pub mod desktop;
pub mod executor;
pub mod injector;
pub mod kwin;
pub mod launcher;
pub mod notify;
pub mod proc;
pub mod screenshot;
pub mod shortcuts;
pub mod tmuxctl;
pub mod windows;

pub use a11y::{A11y, FocusedText, TextHandle};
pub use command::{AppTarget, Command, WindowTarget};
pub use config::DesktopdConfig;
pub use desktop::{DesktopEntry, DesktopIndex};
pub use executor::{DesktopState, Executor, Outcome, Verified, WindowOp};
pub use windows::{Window, WindowError};
