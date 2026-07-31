//! Validated FETCH request arguments and zero-copy typed views.

mod arguments;
mod encode;
mod section;

#[cfg(test)]
mod tests;

pub use arguments::{
    DEFAULT_FETCH_MAX_DEPTH, FetchArguments, FetchAttribute, FetchAttributeIter, FetchMacro,
    FetchModifier, FetchModifierIter,
};
pub use section::{
    FetchHeaderFieldIter, FetchHeaderFields, FetchPartial, FetchSection, FetchSectionPartIter,
    FetchSectionText,
};

pub(crate) use arguments::{
    split_sequence_and_items, validate_fetch_arguments, validate_uid_fetch_arguments,
};
pub(crate) use section::parse_section;
