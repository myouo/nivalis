//! Mail repository facade.
//!
//! The current prototype uses the bounded in-memory implementation. The facade
//! keeps presentation code independent from its file layout so a SQLite-backed
//! implementation can replace it without moving UI bindings again.

mod memory;

pub(crate) use memory::{MailStats, MailStore, MailView};
