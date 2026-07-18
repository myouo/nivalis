//! Mail repository facade.
//!
//! The current prototype uses the bounded in-memory implementation. The facade
//! keeps presentation code independent from its file layout so a SQLite-backed
//! implementation can replace it without moving UI bindings again.

mod memory;
mod path;
// Persistence is active behind the core while the controller remains on one
// consistent in-memory repository until the mutation API is complete.
#[allow(dead_code)]
pub(crate) mod sqlite;

pub(crate) use memory::{MailStats, MailStore, MailView};
pub(crate) use path::database_path;
