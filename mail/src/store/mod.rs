//! Mail repository facade.
//!
//! SQLite owns production mailbox state. The bounded in-memory repository remains
//! available only to its focused unit tests.

#[cfg(test)]
#[allow(dead_code)]
mod memory;
mod path;
// Provider execution is intentionally staged after the local SQLite cutover.
#[allow(dead_code)]
pub(crate) mod sqlite;

pub(crate) use path::database_path;
