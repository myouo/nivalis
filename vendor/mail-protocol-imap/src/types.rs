use bytes::Bytes;

use crate::{
    GetQuotaArguments, GetQuotaRootArguments, SelectArguments, SequenceSet, SequenceSetRef,
    SetQuotaArguments, SortArguments, StoreOperation, ThreadArguments,
};

/// A complete IMAP command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Command {
    /// Client-selected command tag.
    pub tag: Bytes,
    /// Parsed command body.
    pub body: CommandBody,
}

/// Allocation-free view of one complete, semantically validated IMAP command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandRef<'a> {
    /// Client-selected command tag.
    pub tag: &'a [u8],
    /// Parsed command body borrowing the original frame.
    pub body: CommandBodyRef<'a>,
}

/// Borrowed IMAP command body corresponding to [`CommandBody`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CommandBodyRef<'a> {
    /// CAPABILITY.
    Capability,
    /// NOOP.
    Noop,
    /// LOGOUT.
    Logout,
    /// STARTTLS.
    StartTls,
    /// IDLE.
    Idle,
    /// CHECK.
    Check,
    /// CLOSE.
    Close,
    /// EXPUNGE.
    Expunge,
    /// LOGIN with wire-form astrings.
    Login {
        /// Username astring.
        username: &'a [u8],
        /// Password astring.
        password: &'a [u8],
    },
    /// AUTHENTICATE with an optional SASL initial response.
    Authenticate {
        /// SASL mechanism name.
        mechanism: &'a [u8],
        /// Initial response, including `=` for an empty response.
        initial_response: Option<&'a [u8]>,
    },
    /// ENABLE with its validated space-separated capability list.
    Enable { capabilities: &'a [u8] },
    /// SELECT with a wire-form mailbox argument.
    Select { mailbox: &'a [u8] },
    /// SELECT with validated RFC 4466 parameters, including CONDSTORE/QRESYNC.
    SelectExtended { arguments: &'a [u8] },
    /// EXAMINE with a wire-form mailbox argument.
    Examine { mailbox: &'a [u8] },
    /// EXAMINE with validated RFC 4466 parameters, including CONDSTORE/QRESYNC.
    ExamineExtended { arguments: &'a [u8] },
    /// UNSELECT the active mailbox without expunging it.
    Unselect,
    /// CREATE a mailbox.
    Create { mailbox: &'a [u8] },
    /// DELETE a mailbox.
    Delete { mailbox: &'a [u8] },
    /// RENAME a mailbox.
    Rename { from: &'a [u8], to: &'a [u8] },
    /// SUBSCRIBE to a mailbox.
    Subscribe { mailbox: &'a [u8] },
    /// UNSUBSCRIBE from a mailbox.
    Unsubscribe { mailbox: &'a [u8] },
    /// LIST with validated extension-friendly arguments.
    List { arguments: &'a [u8] },
    /// Legacy LSUB with validated arguments.
    Lsub { arguments: &'a [u8] },
    /// NAMESPACE.
    Namespace,
    /// GETQUOTA with a validated quota-root `astring`.
    GetQuota { arguments: &'a [u8] },
    /// GETQUOTAROOT with a validated mailbox `astring`.
    GetQuotaRoot { arguments: &'a [u8] },
    /// SETQUOTA with validated RFC 9208 resource limits.
    SetQuota { arguments: &'a [u8] },
    /// STATUS with a validated mailbox and item list.
    Status { mailbox: &'a [u8], items: &'a [u8] },
    /// APPEND with a validated mailbox and argument sequence.
    Append {
        mailbox: &'a [u8],
        arguments: &'a [u8],
    },
    /// ID command parameters.
    Id { parameters: &'a [u8] },
    /// SEARCH with a validated search program.
    Search { criteria: &'a [u8] },
    /// SORT with validated RFC 5256 arguments.
    Sort { arguments: &'a [u8] },
    /// THREAD with validated RFC 5256 arguments.
    Thread { arguments: &'a [u8] },
    /// FETCH data items for a validated sequence-set.
    Fetch {
        sequence_set: SequenceSetRef<'a>,
        items: &'a [u8],
    },
    /// STORE flags for a validated sequence-set.
    Store {
        sequence_set: SequenceSetRef<'a>,
        operation: StoreOperation,
        silent: bool,
        flags: &'a [u8],
    },
    /// Conditional STORE with the RFC 7162 UNCHANGEDSINCE modifier.
    StoreConditional {
        /// Target message sequence-set or UID set.
        sequence_set: SequenceSetRef<'a>,
        /// Inclusive upper bound for the existing per-message mod-sequence.
        unchanged_since: u64,
        /// Flag replacement/addition/removal operation.
        operation: StoreOperation,
        /// Whether the data item has the `.SILENT` suffix.
        silent: bool,
        /// Validated parenthesized or single flag list.
        flags: &'a [u8],
    },
    /// COPY messages to another mailbox.
    Copy {
        sequence_set: SequenceSetRef<'a>,
        mailbox: &'a [u8],
    },
    /// MOVE messages to another mailbox.
    Move {
        sequence_set: SequenceSetRef<'a>,
        mailbox: &'a [u8],
    },
    /// UID-prefixed command with its subcommand separated from arguments.
    Uid {
        command: &'a [u8],
        arguments: &'a [u8],
    },
    /// A valid framed extension command without a dedicated typed variant.
    Raw { name: &'a [u8], arguments: &'a [u8] },
}

/// Common IMAP commands plus a lossless extension path.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CommandBody {
    /// CAPABILITY.
    Capability,
    /// NOOP.
    Noop,
    /// LOGOUT.
    Logout,
    /// STARTTLS.
    StartTls,
    /// IDLE.
    Idle,
    /// CHECK.
    Check,
    /// CLOSE.
    Close,
    /// EXPUNGE.
    Expunge,
    /// LOGIN with wire-form astrings.
    Login { username: Bytes, password: Bytes },
    /// AUTHENTICATE with an optional SASL initial response.
    Authenticate {
        /// SASL mechanism name.
        mechanism: Bytes,
        /// Initial response, including `=` for an empty response.
        initial_response: Option<Bytes>,
    },
    /// ENABLE one or more capabilities.
    Enable {
        /// Capability names requested by the client.
        capabilities: Vec<Bytes>,
    },
    /// SELECT with a wire-form mailbox argument.
    Select { mailbox: Bytes },
    /// SELECT with typed RFC 4466 parameters, including CONDSTORE/QRESYNC.
    SelectExtended { arguments: SelectArguments },
    /// EXAMINE with a wire-form mailbox argument.
    Examine { mailbox: Bytes },
    /// EXAMINE with typed RFC 4466 parameters, including CONDSTORE/QRESYNC.
    ExamineExtended { arguments: SelectArguments },
    /// UNSELECT the active mailbox without expunging it.
    Unselect,
    /// CREATE a mailbox.
    Create { mailbox: Bytes },
    /// DELETE a mailbox.
    Delete { mailbox: Bytes },
    /// RENAME a mailbox.
    Rename { from: Bytes, to: Bytes },
    /// SUBSCRIBE to a mailbox.
    Subscribe { mailbox: Bytes },
    /// UNSUBSCRIBE from a mailbox.
    Unsubscribe { mailbox: Bytes },
    /// LIST with its extension-friendly argument grammar preserved.
    List { arguments: Bytes },
    /// Legacy LSUB with its argument grammar preserved.
    Lsub { arguments: Bytes },
    /// NAMESPACE.
    Namespace,
    /// GETQUOTA with typed RFC 9208 arguments.
    GetQuota { arguments: GetQuotaArguments },
    /// GETQUOTAROOT with typed RFC 9208 arguments.
    GetQuotaRoot { arguments: GetQuotaRootArguments },
    /// SETQUOTA with typed RFC 9208 resource limits.
    SetQuota { arguments: SetQuotaArguments },
    /// STATUS with a validated mailbox and [`crate::StatusItems`] wire value.
    Status { mailbox: Bytes, items: Bytes },
    /// APPEND with a validated mailbox and [`crate::AppendArguments`] wire value.
    Append { mailbox: Bytes, arguments: Bytes },
    /// ID command parameters, normally NIL or a parenthesized list.
    Id { parameters: Bytes },
    /// SEARCH with a validated [`crate::SearchProgram`] wire value.
    Search { criteria: Bytes },
    /// SORT with typed RFC 5256 arguments.
    Sort { arguments: SortArguments },
    /// THREAD with typed RFC 5256 arguments.
    Thread { arguments: ThreadArguments },
    /// FETCH data items for a parsed sequence-set.
    Fetch {
        sequence_set: SequenceSet,
        items: Bytes,
    },
    /// STORE flags for a parsed sequence-set.
    Store {
        sequence_set: SequenceSet,
        operation: StoreOperation,
        silent: bool,
        flags: Bytes,
    },
    /// Conditional STORE with the RFC 7162 UNCHANGEDSINCE modifier.
    StoreConditional {
        /// Target message sequence-set or UID set.
        sequence_set: SequenceSet,
        /// Inclusive upper bound for the existing per-message mod-sequence.
        unchanged_since: u64,
        /// Flag replacement/addition/removal operation.
        operation: StoreOperation,
        /// Whether the data item has the `.SILENT` suffix.
        silent: bool,
        /// Validated parenthesized or single flag list.
        flags: Bytes,
    },
    /// COPY messages to another mailbox.
    Copy {
        sequence_set: SequenceSet,
        mailbox: Bytes,
    },
    /// MOVE messages to another mailbox.
    Move {
        sequence_set: SequenceSet,
        mailbox: Bytes,
    },
    /// UID-prefixed command with its subcommand separated from arguments.
    Uid { command: Bytes, arguments: Bytes },
    /// A syntactically framed command that is not yet represented as a typed variant.
    Raw {
        /// Command name exactly as received.
        name: Bytes,
        /// Bytes after the command name, excluding the final command CRLF.
        arguments: Bytes,
    },
}

/// An IMAP response frame.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Response {
    /// Continuation request beginning with `+`.
    Continuation { data: Bytes },
    /// Untagged server data beginning with `*`.
    Untagged { data: Bytes },
    /// Tagged command completion response.
    Tagged {
        /// Tag copied from the initiating command.
        tag: Bytes,
        /// Completion status.
        status: Status,
        /// Response text after the status token.
        information: Bytes,
    },
}

/// Tagged IMAP completion status.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum Status {
    /// Command completed successfully.
    Ok,
    /// Command was understood but could not be completed.
    No,
    /// Command was rejected as invalid.
    Bad,
}

/// One client response line in an IMAP AUTHENTICATE exchange.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AuthenticateContinuation {
    /// A syntactically valid RFC 4648 base64 response. An empty value is valid.
    Response(Bytes),
    /// The single `*` line that cancels authentication.
    Cancel,
}

/// The client `DONE` continuation that terminates an IDLE command.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct IdleDone;
