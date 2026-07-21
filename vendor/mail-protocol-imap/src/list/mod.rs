//! Extended LIST command and response validation with zero-copy typed views.

mod arguments;
mod encode;
mod mailbox;
mod options;
mod response;
mod validation;

#[cfg(test)]
mod tests;

pub use arguments::ListArguments;
pub(crate) use arguments::validate_list_arguments;
pub use mailbox::{ListMailbox, ListMailboxIter, ListMailboxKind};
pub use options::{
    ListReturnOption, ListReturnOptionIter, ListSelectionOption, ListSelectionOptionIter,
};
pub use response::{
    ListAttribute, ListAttributeIter, ListExtendedItem, ListExtendedItemIter, ListResponse,
};

/// Default maximum nesting accepted in LIST option and response extension values.
pub const DEFAULT_LIST_MAX_DEPTH: usize = 64;
