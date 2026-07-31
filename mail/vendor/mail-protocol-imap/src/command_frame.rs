use core::ops::Range;

use bytes::Bytes;
use mail_protocol_core::{Limits, ProtocolError};

use crate::codec::own_command;
use crate::{Command, CommandBodyRef, CommandRef, SequenceSetRef, StoreOperation};

/// One semantically validated IMAP command backed by a single owned frame.
///
/// Unlike [`Command`], this representation does not create a separate
/// reference-counted [`Bytes`] owner for every field. Consumers can inspect a
/// complete borrowed view through [`Self::as_ref`] or explicitly convert to the
/// field-owning representation with [`Self::into_command`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandFrame {
    wire: Bytes,
    tag: Range<usize>,
    layout: CommandLayout,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum CommandLayout {
    Capability,
    Noop,
    Logout,
    StartTls,
    Idle,
    Check,
    Close,
    Expunge,
    Login {
        username: Range<usize>,
        password: Range<usize>,
    },
    Authenticate {
        mechanism: Range<usize>,
        initial_response: Option<Range<usize>>,
    },
    Enable(Range<usize>),
    Select(Range<usize>),
    SelectExtended(Range<usize>),
    Examine(Range<usize>),
    ExamineExtended(Range<usize>),
    Unselect,
    Create(Range<usize>),
    Delete(Range<usize>),
    Rename {
        from: Range<usize>,
        to: Range<usize>,
    },
    Subscribe(Range<usize>),
    Unsubscribe(Range<usize>),
    List(Range<usize>),
    Lsub(Range<usize>),
    Namespace,
    GetQuota(Range<usize>),
    GetQuotaRoot(Range<usize>),
    SetQuota(Range<usize>),
    Status {
        mailbox: Range<usize>,
        items: Range<usize>,
    },
    Append {
        mailbox: Range<usize>,
        arguments: Range<usize>,
    },
    Id(Range<usize>),
    Search(Range<usize>),
    Sort(Range<usize>),
    Thread(Range<usize>),
    Fetch {
        sequence_set: Range<usize>,
        saved_search: bool,
        items: Range<usize>,
    },
    Store {
        sequence_set: Range<usize>,
        saved_search: bool,
        operation: StoreOperation,
        silent: bool,
        flags: Range<usize>,
    },
    StoreConditional {
        sequence_set: Range<usize>,
        saved_search: bool,
        unchanged_since: u64,
        operation: StoreOperation,
        silent: bool,
        flags: Range<usize>,
    },
    Copy {
        sequence_set: Range<usize>,
        saved_search: bool,
        mailbox: Range<usize>,
    },
    Move {
        sequence_set: Range<usize>,
        saved_search: bool,
        mailbox: Range<usize>,
    },
    Uid {
        command: Range<usize>,
        arguments: Range<usize>,
    },
    Raw {
        name: Range<usize>,
        arguments: Range<usize>,
    },
}

impl CommandFrame {
    /// Parses exactly one complete command into a one-backing owned frame.
    ///
    /// # Errors
    ///
    /// Returns an error for incomplete or trailing frames, invalid literal
    /// framing, exceeded limits, or invalid command syntax and semantics.
    pub fn parse(wire: Bytes) -> Result<Self, ProtocolError> {
        Self::parse_with_limits(wire, Limits::default())
    }

    /// Parses one complete command with explicit resource limits.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::parse`] using `limits` for framing,
    /// literal size, and recursive syntax budgets.
    pub fn parse_with_limits(wire: Bytes, limits: Limits) -> Result<Self, ProtocolError> {
        let command = CommandRef::parse_with_limits(&wire, limits)?;
        let tag = subslice_range(&wire, command.tag);
        let layout = CommandLayout::from_ref(&wire, command.body);
        Ok(Self { wire, tag, layout })
    }

    /// Returns the exact validated command frame, including its final CRLF.
    pub const fn as_bytes(&self) -> &Bytes {
        &self.wire
    }

    /// Returns an allocation-free command view borrowing this owned frame.
    pub fn as_ref(&self) -> CommandRef<'_> {
        CommandRef {
            tag: &self.wire[self.tag.clone()],
            body: self.layout.as_ref(&self.wire),
        }
    }

    /// Converts to the field-owning [`Command`] representation.
    ///
    /// Reference-counted field subviews and owning sequence-set vectors are
    /// created only when this conversion is requested.
    pub fn into_command(self) -> Command {
        own_command(&self.wire, self.as_ref())
    }
}

impl CommandLayout {
    #[allow(clippy::too_many_lines)]
    fn from_ref(frame: &[u8], body: CommandBodyRef<'_>) -> Self {
        match body {
            CommandBodyRef::Capability => Self::Capability,
            CommandBodyRef::Noop => Self::Noop,
            CommandBodyRef::Logout => Self::Logout,
            CommandBodyRef::StartTls => Self::StartTls,
            CommandBodyRef::Idle => Self::Idle,
            CommandBodyRef::Check => Self::Check,
            CommandBodyRef::Close => Self::Close,
            CommandBodyRef::Expunge => Self::Expunge,
            CommandBodyRef::Login { username, password } => Self::Login {
                username: subslice_range(frame, username),
                password: subslice_range(frame, password),
            },
            CommandBodyRef::Authenticate {
                mechanism,
                initial_response,
            } => Self::Authenticate {
                mechanism: subslice_range(frame, mechanism),
                initial_response: initial_response.map(|value| subslice_range(frame, value)),
            },
            CommandBodyRef::Enable { capabilities } => {
                Self::Enable(subslice_range(frame, capabilities))
            }
            CommandBodyRef::Select { mailbox } => Self::Select(subslice_range(frame, mailbox)),
            CommandBodyRef::SelectExtended { arguments } => {
                Self::SelectExtended(subslice_range(frame, arguments))
            }
            CommandBodyRef::Examine { mailbox } => Self::Examine(subslice_range(frame, mailbox)),
            CommandBodyRef::ExamineExtended { arguments } => {
                Self::ExamineExtended(subslice_range(frame, arguments))
            }
            CommandBodyRef::Unselect => Self::Unselect,
            CommandBodyRef::Create { mailbox } => Self::Create(subslice_range(frame, mailbox)),
            CommandBodyRef::Delete { mailbox } => Self::Delete(subslice_range(frame, mailbox)),
            CommandBodyRef::Rename { from, to } => Self::Rename {
                from: subslice_range(frame, from),
                to: subslice_range(frame, to),
            },
            CommandBodyRef::Subscribe { mailbox } => {
                Self::Subscribe(subslice_range(frame, mailbox))
            }
            CommandBodyRef::Unsubscribe { mailbox } => {
                Self::Unsubscribe(subslice_range(frame, mailbox))
            }
            CommandBodyRef::List { arguments } => Self::List(subslice_range(frame, arguments)),
            CommandBodyRef::Lsub { arguments } => Self::Lsub(subslice_range(frame, arguments)),
            CommandBodyRef::Namespace => Self::Namespace,
            CommandBodyRef::GetQuota { arguments } => {
                Self::GetQuota(subslice_range(frame, arguments))
            }
            CommandBodyRef::GetQuotaRoot { arguments } => {
                Self::GetQuotaRoot(subslice_range(frame, arguments))
            }
            CommandBodyRef::SetQuota { arguments } => {
                Self::SetQuota(subslice_range(frame, arguments))
            }
            CommandBodyRef::Status { mailbox, items } => Self::Status {
                mailbox: subslice_range(frame, mailbox),
                items: subslice_range(frame, items),
            },
            CommandBodyRef::Append { mailbox, arguments } => Self::Append {
                mailbox: subslice_range(frame, mailbox),
                arguments: subslice_range(frame, arguments),
            },
            CommandBodyRef::Id { parameters } => Self::Id(subslice_range(frame, parameters)),
            CommandBodyRef::Search { criteria } => Self::Search(subslice_range(frame, criteria)),
            CommandBodyRef::Sort { arguments } => Self::Sort(subslice_range(frame, arguments)),
            CommandBodyRef::Thread { arguments } => Self::Thread(subslice_range(frame, arguments)),
            CommandBodyRef::Fetch {
                sequence_set,
                items,
            } => Self::Fetch {
                sequence_set: subslice_range(frame, sequence_set.as_bytes()),
                saved_search: sequence_set.is_saved_search(),
                items: subslice_range(frame, items),
            },
            CommandBodyRef::Store {
                sequence_set,
                operation,
                silent,
                flags,
            } => Self::Store {
                sequence_set: subslice_range(frame, sequence_set.as_bytes()),
                saved_search: sequence_set.is_saved_search(),
                operation,
                silent,
                flags: subslice_range(frame, flags),
            },
            CommandBodyRef::StoreConditional {
                sequence_set,
                unchanged_since,
                operation,
                silent,
                flags,
            } => Self::StoreConditional {
                sequence_set: subslice_range(frame, sequence_set.as_bytes()),
                saved_search: sequence_set.is_saved_search(),
                unchanged_since,
                operation,
                silent,
                flags: subslice_range(frame, flags),
            },
            CommandBodyRef::Copy {
                sequence_set,
                mailbox,
            } => Self::Copy {
                sequence_set: subslice_range(frame, sequence_set.as_bytes()),
                saved_search: sequence_set.is_saved_search(),
                mailbox: subslice_range(frame, mailbox),
            },
            CommandBodyRef::Move {
                sequence_set,
                mailbox,
            } => Self::Move {
                sequence_set: subslice_range(frame, sequence_set.as_bytes()),
                saved_search: sequence_set.is_saved_search(),
                mailbox: subslice_range(frame, mailbox),
            },
            CommandBodyRef::Uid { command, arguments } => Self::Uid {
                command: subslice_range(frame, command),
                arguments: subslice_range(frame, arguments),
            },
            CommandBodyRef::Raw { name, arguments } => Self::Raw {
                name: subslice_range(frame, name),
                arguments: subslice_range(frame, arguments),
            },
        }
    }

    #[allow(clippy::too_many_lines)]
    fn as_ref<'a>(&self, frame: &'a [u8]) -> CommandBodyRef<'a> {
        match self {
            Self::Capability => CommandBodyRef::Capability,
            Self::Noop => CommandBodyRef::Noop,
            Self::Logout => CommandBodyRef::Logout,
            Self::StartTls => CommandBodyRef::StartTls,
            Self::Idle => CommandBodyRef::Idle,
            Self::Check => CommandBodyRef::Check,
            Self::Close => CommandBodyRef::Close,
            Self::Expunge => CommandBodyRef::Expunge,
            Self::Login { username, password } => CommandBodyRef::Login {
                username: &frame[username.clone()],
                password: &frame[password.clone()],
            },
            Self::Authenticate {
                mechanism,
                initial_response,
            } => CommandBodyRef::Authenticate {
                mechanism: &frame[mechanism.clone()],
                initial_response: initial_response.as_ref().map(|range| &frame[range.clone()]),
            },
            Self::Enable(capabilities) => CommandBodyRef::Enable {
                capabilities: &frame[capabilities.clone()],
            },
            Self::Select(mailbox) => CommandBodyRef::Select {
                mailbox: &frame[mailbox.clone()],
            },
            Self::SelectExtended(arguments) => CommandBodyRef::SelectExtended {
                arguments: &frame[arguments.clone()],
            },
            Self::Examine(mailbox) => CommandBodyRef::Examine {
                mailbox: &frame[mailbox.clone()],
            },
            Self::ExamineExtended(arguments) => CommandBodyRef::ExamineExtended {
                arguments: &frame[arguments.clone()],
            },
            Self::Unselect => CommandBodyRef::Unselect,
            Self::Create(mailbox) => CommandBodyRef::Create {
                mailbox: &frame[mailbox.clone()],
            },
            Self::Delete(mailbox) => CommandBodyRef::Delete {
                mailbox: &frame[mailbox.clone()],
            },
            Self::Rename { from, to } => CommandBodyRef::Rename {
                from: &frame[from.clone()],
                to: &frame[to.clone()],
            },
            Self::Subscribe(mailbox) => CommandBodyRef::Subscribe {
                mailbox: &frame[mailbox.clone()],
            },
            Self::Unsubscribe(mailbox) => CommandBodyRef::Unsubscribe {
                mailbox: &frame[mailbox.clone()],
            },
            Self::List(arguments) => CommandBodyRef::List {
                arguments: &frame[arguments.clone()],
            },
            Self::Lsub(arguments) => CommandBodyRef::Lsub {
                arguments: &frame[arguments.clone()],
            },
            Self::Namespace => CommandBodyRef::Namespace,
            Self::GetQuota(arguments) => CommandBodyRef::GetQuota {
                arguments: &frame[arguments.clone()],
            },
            Self::GetQuotaRoot(arguments) => CommandBodyRef::GetQuotaRoot {
                arguments: &frame[arguments.clone()],
            },
            Self::SetQuota(arguments) => CommandBodyRef::SetQuota {
                arguments: &frame[arguments.clone()],
            },
            Self::Status { mailbox, items } => CommandBodyRef::Status {
                mailbox: &frame[mailbox.clone()],
                items: &frame[items.clone()],
            },
            Self::Append { mailbox, arguments } => CommandBodyRef::Append {
                mailbox: &frame[mailbox.clone()],
                arguments: &frame[arguments.clone()],
            },
            Self::Id(parameters) => CommandBodyRef::Id {
                parameters: &frame[parameters.clone()],
            },
            Self::Search(criteria) => CommandBodyRef::Search {
                criteria: &frame[criteria.clone()],
            },
            Self::Sort(arguments) => CommandBodyRef::Sort {
                arguments: &frame[arguments.clone()],
            },
            Self::Thread(arguments) => CommandBodyRef::Thread {
                arguments: &frame[arguments.clone()],
            },
            Self::Fetch {
                sequence_set,
                saved_search,
                items,
            } => CommandBodyRef::Fetch {
                sequence_set: validated_sequence_ref(frame, sequence_set, *saved_search),
                items: &frame[items.clone()],
            },
            Self::Store {
                sequence_set,
                saved_search,
                operation,
                silent,
                flags,
            } => CommandBodyRef::Store {
                sequence_set: validated_sequence_ref(frame, sequence_set, *saved_search),
                operation: *operation,
                silent: *silent,
                flags: &frame[flags.clone()],
            },
            Self::StoreConditional {
                sequence_set,
                saved_search,
                unchanged_since,
                operation,
                silent,
                flags,
            } => CommandBodyRef::StoreConditional {
                sequence_set: validated_sequence_ref(frame, sequence_set, *saved_search),
                unchanged_since: *unchanged_since,
                operation: *operation,
                silent: *silent,
                flags: &frame[flags.clone()],
            },
            Self::Copy {
                sequence_set,
                saved_search,
                mailbox,
            } => CommandBodyRef::Copy {
                sequence_set: validated_sequence_ref(frame, sequence_set, *saved_search),
                mailbox: &frame[mailbox.clone()],
            },
            Self::Move {
                sequence_set,
                saved_search,
                mailbox,
            } => CommandBodyRef::Move {
                sequence_set: validated_sequence_ref(frame, sequence_set, *saved_search),
                mailbox: &frame[mailbox.clone()],
            },
            Self::Uid { command, arguments } => CommandBodyRef::Uid {
                command: &frame[command.clone()],
                arguments: &frame[arguments.clone()],
            },
            Self::Raw { name, arguments } => CommandBodyRef::Raw {
                name: &frame[name.clone()],
                arguments: &frame[arguments.clone()],
            },
        }
    }
}

fn validated_sequence_ref<'a>(
    frame: &'a [u8],
    range: &Range<usize>,
    saved_search: bool,
) -> SequenceSetRef<'a> {
    SequenceSetRef::from_validated(&frame[range.clone()], saved_search)
}

fn subslice_range(frame: &[u8], value: &[u8]) -> Range<usize> {
    if value.is_empty() {
        return frame.len()..frame.len();
    }
    let start = (value.as_ptr() as usize).wrapping_sub(frame.as_ptr() as usize);
    let end = start.wrapping_add(value.len());
    debug_assert!(
        start <= frame.len(),
        "validated IMAP field starts inside its frame"
    );
    debug_assert!(end >= start, "validated IMAP field range does not overflow");
    debug_assert!(
        end <= frame.len(),
        "validated IMAP field ends inside its frame"
    );
    start..end
}
