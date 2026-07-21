use core::time::Duration;

use bytes::Bytes;

use crate::{CapabilitySet, ESearchResponse, ListResponse, SavedSearchUpdate, Status};

mod client;
mod enable;
mod semantics;
mod server;

pub use client::ClientSession;
pub use server::{ServerSession, ServerSessionEvent};

pub(crate) use semantics::{
    CommandSemantics, classify_command, enable_capabilities, information_has_code, invalid_state,
    validate_extension_requirements,
};

/// Recommended maximum interval between terminating and reissuing IDLE.
///
/// RFC 2177 advises clients to renew IDLE at least every 29 minutes so that a
/// server with a 30-minute inactivity timeout does not implicitly log them off.
/// This crate exposes the protocol decision value but does not own a timer.
pub const IDLE_REISSUE_INTERVAL: Duration = Duration::from_secs(29 * 60);

/// Authentication/mailbox state of an IMAP client session.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum SessionState {
    /// No server greeting has been processed.
    AwaitingGreeting,
    /// The connection is established but not authenticated.
    NotAuthenticated,
    /// The connection is authenticated without a selected mailbox.
    Authenticated,
    /// A mailbox is selected.
    Selected {
        /// Whether the mailbox was selected read-only.
        read_only: bool,
    },
    /// No more commands may be submitted.
    Logout,
}

/// Transport protection state tracked independently from authentication.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SecurityState {
    /// The connection has not completed a TLS upgrade.
    Plaintext,
    /// STARTTLS was accepted, but the external TLS handshake has not completed.
    TlsHandshake,
    /// The connection is protected by TLS.
    Tls,
}

/// Semantic class of an in-flight IMAP command.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum PendingCommand {
    /// CAPABILITY.
    Capability,
    /// NOOP.
    Noop,
    /// LOGOUT.
    Logout,
    /// STARTTLS.
    StartTls,
    /// LOGIN.
    Login,
    /// AUTHENTICATE.
    Authenticate,
    /// ENABLE.
    Enable,
    /// SELECT.
    Select,
    /// EXAMINE.
    Examine,
    /// CLOSE or UNSELECT.
    Deselect,
    /// IDLE.
    Idle,
    /// SEARCH.
    Search,
    /// UID SEARCH.
    UidSearch,
    /// LIST.
    List,
    /// A command valid only in selected state.
    SelectedOperation,
    /// A command valid in authenticated or selected state.
    AuthenticatedOperation,
    /// An extension whose state semantics are owned by the caller.
    Extension,
}

/// Observable result of processing one server response.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SessionEvent {
    /// The initial server greeting established a state.
    Greeting {
        /// State selected by OK, PREAUTH, or BYE.
        state: SessionState,
    },
    /// An untagged CAPABILITY response replaced the cached capabilities.
    CapabilitiesUpdated,
    /// An ENABLED response reported extensions activated for a pending ENABLE.
    CapabilitiesEnabled {
        /// Capabilities activated by this specific ENABLE command.
        capabilities: CapabilitySet,
    },
    /// The server requested continuation data for an exclusive command.
    Continuation {
        /// Command waiting for continuation data.
        command: PendingCommand,
        /// Server challenge or continuation text.
        data: Bytes,
    },
    /// A tagged response completed a command.
    Completed {
        /// Completed command tag.
        tag: Bytes,
        /// Semantic command class.
        command: PendingCommand,
        /// Tagged completion status.
        status: Status,
        /// Effect of this completion on the RFC 9051 `$` variable.
        saved_search: SavedSearchUpdate,
    },
    /// Unsolicited server data did not directly change session state.
    Unsolicited,
    /// The server sent BYE and the session entered logout state.
    ServerBye,
    /// An unsolicited CLOSED response code deselected the mailbox.
    MailboxClosed,
    /// A new UIDVALIDITY value invalidated the RFC 9051 `$` variable.
    SavedSearchReset,
    /// Typed ESEARCH data, optionally correlated to an in-flight SEARCH command.
    SearchResults {
        /// Validated zero-copy result data.
        response: ESearchResponse,
    },
    /// Typed LIST data with the strongest correlation available on the wire.
    ListData {
        /// Validated zero-copy LIST response data.
        response: ListResponse,
        /// Relationship to currently in-flight LIST commands.
        correlation: ListCorrelation,
    },
}

/// Correlation available for an untagged LIST response.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ListCorrelation {
    /// The response has no CHILDINFO criteria, so solicited and unsolicited
    /// data cannot be distinguished on the wire.
    Unspecified,
    /// CHILDINFO uniquely identifies one in-flight LIST command.
    Matched {
        /// Tag of the matching LIST command.
        tag: Bytes,
    },
    /// CHILDINFO matches multiple in-flight LIST commands with equivalent
    /// base selection criteria.
    Ambiguous,
}
