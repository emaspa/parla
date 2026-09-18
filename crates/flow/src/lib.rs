//! parla-flow: the user's side of dictation. The dictionary of words they
//! use, the snippets they insert by name, how each application wants its
//! text, and the history of what was dictated. The daemon reads these to
//! shape every utterance; the UI edits the same files and asks the daemon
//! to reload.
//!
//! All three editable files live under [`paths::config_dir`] as TOML, so a
//! text editor works as well as the UI. History is an append-only JSONL
//! under [`paths::data_dir`], next to the spellings the daemon learned from
//! corrections ([`learned`]), which the UI offers for the dictionary.

pub mod apps;
pub mod dictionary;
pub mod history;
pub mod learned;
pub mod paths;
pub mod snippets;
pub mod text;

pub use apps::{AppProfile, AppProfiles, Tone};
pub use dictionary::{Dictionary, Replacement};
pub use history::{History, Record, Stats};
pub use learned::{Suggestion, Suggestions};
pub use snippets::{Snippet, Snippets};
