//! Mail repository facade.
//!
//! The current prototype uses the bounded in-memory implementation. The facade
//! keeps presentation code independent from its file layout so a SQLite-backed
//! implementation can replace it without moving UI bindings again.

mod memory;
// Persistence lands incrementally before the controller switches data sources.
#[allow(dead_code)]
mod sqlite;

pub(crate) use memory::{MailStats, MailStore, MailView};
