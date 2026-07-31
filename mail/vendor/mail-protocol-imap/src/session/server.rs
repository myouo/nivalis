use bytes::Bytes;
use mail_protocol_core::ProtocolError;

use crate::{
    AuthenticateContinuation, Capability, CapabilitySet, Command, IdleDone, ListArguments,
    PendingCommand, Response, ResponseCode, SavedSearchScope, SavedSearchUpdate, SecurityState,
    SessionState, Status, StatusKind, UntaggedData,
    codec::validate_base64,
    parse_untagged,
    session::{
        CommandSemantics, classify_command, enable_capabilities, information_has_code,
        invalid_state, validate_extension_requirements,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ContinuationPhase {
    None,
    AuthenticateReady,
    AuthenticateWaiting,
    AuthenticateCancelled,
    IdleReady,
    IdleWaiting,
    IdleDone,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InFlight {
    tag: Bytes,
    command: PendingCommand,
    continuation: ContinuationPhase,
    saved_result_scope: Option<SavedSearchScope>,
    list_arguments: Option<ListArguments>,
    list_prefilter: u64,
    enable_requested: Option<CapabilitySet>,
    enable_response: Option<CapabilitySet>,
}

/// Observable result of recording one server response.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ServerSessionEvent {
    /// The initial greeting established the connection state.
    Greeting {
        /// State selected by OK, PREAUTH, or BYE.
        state: SessionState,
    },
    /// A continuation request was sent for AUTHENTICATE or IDLE.
    Continuation {
        /// Command that owns the continuation exchange.
        command: PendingCommand,
    },
    /// A tagged response completed the current command.
    Completed {
        /// Tag copied from the initiating command.
        tag: Bytes,
        /// Semantic command class.
        command: PendingCommand,
        /// Tagged completion status.
        status: Status,
        /// Effect of this completion on the RFC 9051 `$` variable.
        saved_search: SavedSearchUpdate,
    },
    /// A CAPABILITY response changed the set known to have been advertised.
    CapabilitiesAdvertised,
    /// An ENABLED response recorded extensions activated by the pending ENABLE.
    CapabilitiesEnabled {
        /// Capabilities activated by this specific ENABLE command.
        capabilities: CapabilitySet,
    },
    /// A new UIDVALIDITY value invalidated the RFC 9051 `$` variable.
    SavedSearchReset,
    /// Untagged data was sent without completing a command.
    Untagged,
    /// An untagged BYE moved the connection into logout state.
    ServerBye,
}

/// Runtime-independent IMAP server protocol state machine.
///
/// The state machine validates server-visible command ordering and the responses
/// emitted by the caller. It deliberately owns no socket, TLS implementation,
/// SASL mechanism, mailbox storage, authorization policy, or timeout.
///
/// One command is admitted to the semantic execution slot at a time. A caller
/// may continue framing pipelined commands into its own bounded queue, but must
/// not treat an [`ErrorKind::InvalidState`](mail_protocol_core::ErrorKind::InvalidState)
/// caused by a busy slot as a wire-level BAD response. This serial execution is
/// always permitted by RFC 9051 and avoids executing state-dependent pipelined
/// commands before an earlier completion establishes their state.
#[derive(Clone, Debug)]
pub struct ServerSession {
    state: SessionState,
    security: SecurityState,
    advertised_capabilities: CapabilitySet,
    enabled_capabilities: CapabilitySet,
    pending: Option<InFlight>,
    bye_sent: bool,
}

impl ServerSession {
    /// Creates a plaintext session waiting for its initial greeting.
    pub const fn new(advertised_capabilities: CapabilitySet) -> Self {
        Self {
            state: SessionState::AwaitingGreeting,
            security: SecurityState::Plaintext,
            advertised_capabilities,
            enabled_capabilities: CapabilitySet::new(),
            pending: None,
            bye_sent: false,
        }
    }

    /// Creates an implicit-TLS session waiting for its initial greeting.
    pub fn new_tls(mut advertised_capabilities: CapabilitySet) -> Self {
        advertised_capabilities.remove(&Capability::StartTls);
        Self {
            state: SessionState::AwaitingGreeting,
            security: SecurityState::Tls,
            advertised_capabilities,
            enabled_capabilities: CapabilitySet::default(),
            pending: None,
            bye_sent: false,
        }
    }

    /// Returns the confirmed authentication/mailbox state.
    pub const fn state(&self) -> SessionState {
        self.state
    }

    /// Returns the confirmed transport protection state.
    pub const fn security(&self) -> SecurityState {
        self.security
    }

    /// Returns the capabilities configured for the next advertisement.
    pub const fn advertised_capabilities(&self) -> &CapabilitySet {
        &self.advertised_capabilities
    }

    /// Returns extensions committed by successful ENABLE commands.
    pub const fn enabled_capabilities(&self) -> &CapabilitySet {
        &self.enabled_capabilities
    }

    /// Replaces the capabilities used for subsequent command validation.
    ///
    /// STARTTLS is removed automatically once TLS has been established.
    pub fn set_advertised_capabilities(&mut self, mut capabilities: CapabilitySet) {
        if self.security == SecurityState::Tls {
            capabilities.remove(&Capability::StartTls);
        }
        self.advertised_capabilities = capabilities;
    }

    /// Returns the command currently occupying the semantic execution slot.
    pub fn pending_command(&self) -> Option<PendingCommand> {
        self.pending.as_ref().map(|pending| pending.command)
    }

    /// Returns the current command tag, if any.
    pub fn pending_tag(&self) -> Option<&Bytes> {
        self.pending.as_ref().map(|pending| &pending.tag)
    }

    /// Records the initial greeting sent by the server.
    ///
    /// # Errors
    ///
    /// Returns an error unless this is the first response and it is an untagged
    /// OK, PREAUTH, or BYE greeting.
    pub fn on_greeting_sent(
        &mut self,
        response: &Response,
    ) -> Result<ServerSessionEvent, ProtocolError> {
        if self.state != SessionState::AwaitingGreeting || self.pending.is_some() {
            return Err(invalid_state("duplicate IMAP server greeting"));
        }
        let Some(UntaggedData::Status(status)) = parse_untagged(response)? else {
            return Err(invalid_state("invalid IMAP server greeting kind"));
        };
        self.state = match status.kind {
            StatusKind::Ok => SessionState::NotAuthenticated,
            StatusKind::Preauth => SessionState::Authenticated,
            StatusKind::Bye => SessionState::Logout,
            StatusKind::No | StatusKind::Bad => {
                return Err(invalid_state("invalid IMAP server greeting status"));
            }
        };
        if let Some(ResponseCode::Capability(capabilities)) = status.code {
            self.set_advertised_capabilities(capabilities);
        }
        self.bye_sent = status.kind == StatusKind::Bye;
        Ok(ServerSessionEvent::Greeting { state: self.state })
    }

    /// Validates a complete decoded command before application processing.
    ///
    /// # Errors
    ///
    /// Returns an invalid-state error before the greeting, after logout, during
    /// TLS/continuation processing, while another command occupies the semantic
    /// slot, when a manually constructed LIST is malformed, or when the command
    /// is unavailable in the current state.
    pub fn on_command(&mut self, command: &Command) -> Result<PendingCommand, ProtocolError> {
        if matches!(
            self.state,
            SessionState::AwaitingGreeting | SessionState::Logout
        ) {
            return Err(invalid_state(
                "IMAP server command before greeting or after logout",
            ));
        }
        if self.security == SecurityState::TlsHandshake {
            return Err(invalid_state("IMAP server command during TLS handshake"));
        }
        if self.pending.is_some() {
            return Err(invalid_state("IMAP server semantic command slot busy"));
        }

        let semantics: CommandSemantics = classify_command(command);
        let enable_requested = enable_capabilities(command)?;
        let list_arguments = command.parsed_list_arguments()?;
        let list_prefilter = list_arguments
            .as_ref()
            .map_or(0, ListArguments::child_info_prefilter);
        let kind = semantics.command;
        validate_server_command(
            kind,
            self.state,
            self.security,
            &self.advertised_capabilities,
        )?;
        validate_extension_requirements(
            command,
            &self.advertised_capabilities,
            &self.enabled_capabilities,
        )?;
        let continuation = match kind {
            PendingCommand::Authenticate => ContinuationPhase::AuthenticateReady,
            PendingCommand::Idle => ContinuationPhase::IdleReady,
            _ => ContinuationPhase::None,
        };
        self.pending = Some(InFlight {
            tag: command.tag.clone(),
            command: kind,
            continuation,
            saved_result_scope: semantics.saved_result_scope,
            list_arguments,
            list_prefilter,
            enable_requested,
            enable_response: None,
        });
        Ok(kind)
    }

    /// Records one response actually emitted by the server.
    ///
    /// Untagged responses leave the command in flight. A continuation is valid
    /// only for AUTHENTICATE or IDLE. A tagged response must use the pending tag
    /// and obey the active continuation phase.
    ///
    /// # Errors
    ///
    /// Returns an error for a response that would violate the session state.
    pub fn on_response_sent(
        &mut self,
        response: &Response,
    ) -> Result<ServerSessionEvent, ProtocolError> {
        if self.state == SessionState::AwaitingGreeting {
            return self.on_greeting_sent(response);
        }
        if self.security == SecurityState::TlsHandshake {
            return Err(invalid_state("IMAP server response during TLS handshake"));
        }
        if self.state == SessionState::Logout && self.pending.is_none() {
            return Err(invalid_state("IMAP server response after logout"));
        }

        match response {
            Response::Continuation { data } => self.on_continuation_sent(data),
            Response::Untagged { .. } => self.on_untagged_sent(response),
            Response::Tagged {
                tag,
                status,
                information,
            } => self.on_completion_sent(tag, *status, information),
        }
    }

    /// Records a decoded client response to an AUTHENTICATE challenge.
    ///
    /// A normal response permits another challenge or a tagged completion. A
    /// cancellation requires the server to finish the command with tagged BAD.
    ///
    /// # Errors
    ///
    /// Returns an error unless an AUTHENTICATE continuation is outstanding.
    pub fn on_authenticate_continuation(
        &mut self,
        continuation: &AuthenticateContinuation,
    ) -> Result<(), ProtocolError> {
        let Some(pending) = self.pending.as_mut() else {
            return Err(invalid_state("IMAP AUTHENTICATE response without command"));
        };
        if pending.command != PendingCommand::Authenticate
            || pending.continuation != ContinuationPhase::AuthenticateWaiting
        {
            return Err(invalid_state("unexpected IMAP AUTHENTICATE response"));
        }
        pending.continuation = match continuation {
            AuthenticateContinuation::Response(data) => {
                validate_base64(data)?;
                ContinuationPhase::AuthenticateReady
            }
            AuthenticateContinuation::Cancel => ContinuationPhase::AuthenticateCancelled,
        };
        Ok(())
    }

    /// Records the decoded `DONE` line that terminates IDLE.
    ///
    /// # Errors
    ///
    /// Returns an error unless the IDLE continuation has been sent and is still
    /// awaiting `DONE`.
    pub fn on_idle_done(&mut self, _: IdleDone) -> Result<(), ProtocolError> {
        let Some(pending) = self.pending.as_mut() else {
            return Err(invalid_state("IMAP IDLE DONE without command"));
        };
        if pending.command != PendingCommand::Idle
            || pending.continuation != ContinuationPhase::IdleWaiting
        {
            return Err(invalid_state("unexpected IMAP IDLE DONE"));
        }
        pending.continuation = ContinuationPhase::IdleDone;
        Ok(())
    }

    /// Confirms that the external STARTTLS handshake completed successfully.
    ///
    /// # Errors
    ///
    /// Returns an error unless tagged STARTTLS OK has started the handshake.
    pub fn on_tls_established(&mut self) -> Result<(), ProtocolError> {
        if self.security != SecurityState::TlsHandshake || self.pending.is_some() {
            return Err(invalid_state(
                "IMAP server TLS established outside handshake",
            ));
        }
        self.security = SecurityState::Tls;
        self.state = SessionState::NotAuthenticated;
        self.advertised_capabilities.remove(&Capability::StartTls);
        self.enabled_capabilities = CapabilitySet::default();
        Ok(())
    }

    /// Marks the external STARTTLS handshake as failed and closes the session.
    ///
    /// # Errors
    ///
    /// Returns an error unless tagged STARTTLS OK has started the handshake.
    pub fn on_tls_failed(&mut self) -> Result<(), ProtocolError> {
        if self.security != SecurityState::TlsHandshake || self.pending.is_some() {
            return Err(invalid_state("IMAP server TLS failure outside handshake"));
        }
        self.state = SessionState::Logout;
        Ok(())
    }

    fn on_continuation_sent(&mut self, data: &[u8]) -> Result<ServerSessionEvent, ProtocolError> {
        let Some(pending) = self.pending.as_mut() else {
            return Err(invalid_state("IMAP continuation without command"));
        };
        pending.continuation = match pending.continuation {
            ContinuationPhase::AuthenticateReady => {
                validate_base64(data)?;
                ContinuationPhase::AuthenticateWaiting
            }
            ContinuationPhase::IdleReady => ContinuationPhase::IdleWaiting,
            _ => return Err(invalid_state("unexpected IMAP server continuation")),
        };
        Ok(ServerSessionEvent::Continuation {
            command: pending.command,
        })
    }

    fn on_untagged_sent(
        &mut self,
        response: &Response,
    ) -> Result<ServerSessionEvent, ProtocolError> {
        match parse_untagged(response)? {
            Some(UntaggedData::Capability(capabilities)) => {
                self.set_advertised_capabilities(capabilities);
                Ok(ServerSessionEvent::CapabilitiesAdvertised)
            }
            Some(UntaggedData::Enabled(capabilities)) => {
                let Some(pending) = self.pending.as_mut() else {
                    return Err(invalid_state("IMAP ENABLED without pending ENABLE"));
                };
                if pending.command != PendingCommand::Enable || pending.enable_response.is_some() {
                    return Err(invalid_state("unexpected or duplicate IMAP ENABLED"));
                }
                let Some(requested) = pending.enable_requested.as_ref() else {
                    return Err(invalid_state(
                        "IMAP ENABLE command lost requested capabilities",
                    ));
                };
                if capabilities.iter().any(|capability| {
                    !requested.contains(capability)
                        || !self.advertised_capabilities.contains(capability)
                }) {
                    return Err(invalid_state(
                        "IMAP ENABLED capability was not requested and advertised",
                    ));
                }
                pending.enable_response = Some(capabilities.clone());
                Ok(ServerSessionEvent::CapabilitiesEnabled { capabilities })
            }
            Some(UntaggedData::Status(status)) if status.kind == StatusKind::Bye => {
                self.state = SessionState::Logout;
                self.bye_sent = true;
                Ok(ServerSessionEvent::ServerBye)
            }
            Some(UntaggedData::Status(status)) if status.kind == StatusKind::Preauth => {
                Err(invalid_state("IMAP PREAUTH outside greeting"))
            }
            Some(UntaggedData::Status(status))
                if matches!(status.code, Some(ResponseCode::UidValidity(_)))
                    && matches!(self.state, SessionState::Selected { .. })
                    && self.pending.as_ref().is_none_or(|pending| {
                        !matches!(
                            pending.command,
                            PendingCommand::Select | PendingCommand::Examine
                        )
                    }) =>
            {
                Ok(ServerSessionEvent::SavedSearchReset)
            }
            Some(UntaggedData::ESearch(result)) => {
                if let Some(tag) = result.tag() {
                    let tag = tag.decoded();
                    let Some(pending) = self.pending.as_ref() else {
                        return Err(invalid_state("IMAP ESEARCH without pending command"));
                    };
                    if pending.tag.as_ref() != tag.as_ref() {
                        return Err(invalid_state("mismatched IMAP server ESEARCH tag"));
                    }
                    let valid = matches!(
                        (pending.command, result.is_uid()),
                        (PendingCommand::Search, false) | (PendingCommand::UidSearch, true)
                    );
                    if !valid {
                        return Err(invalid_state("mismatched IMAP server ESEARCH kind"));
                    }
                }
                Ok(ServerSessionEvent::Untagged)
            }
            Some(UntaggedData::List(response)) => {
                if let Some(response_prefilter) = response.child_info_prefilter()
                    && self.pending.as_ref().is_none_or(|pending| {
                        response_prefilter & !pending.list_prefilter != 0
                            || pending
                                .list_arguments
                                .as_ref()
                                .is_none_or(|arguments| !arguments.correlates_child_info(&response))
                    })
                {
                    return Err(invalid_state(
                        "IMAP server CHILDINFO without matching LIST command",
                    ));
                }
                Ok(ServerSessionEvent::Untagged)
            }
            Some(UntaggedData::Vanished { .. })
                if !self.enabled_capabilities.contains(&Capability::QResync) =>
            {
                Err(invalid_state("IMAP VANISHED before ENABLE QRESYNC"))
            }
            _ => Ok(ServerSessionEvent::Untagged),
        }
    }

    fn on_completion_sent(
        &mut self,
        tag: &Bytes,
        status: Status,
        information: &[u8],
    ) -> Result<ServerSessionEvent, ProtocolError> {
        let Some(pending) = self.pending.as_ref() else {
            return Err(invalid_state("IMAP tagged response without command"));
        };
        if pending.tag != *tag {
            return Err(invalid_state("mismatched IMAP server completion tag"));
        }
        validate_completion(pending, status, self.bye_sent)?;
        validate_enable_completion(pending, status)?;

        let merged_enabled = if status == Status::Ok
            && pending.command == PendingCommand::Enable
            && let Some(capabilities) = pending.enable_response.as_ref()
        {
            let mut merged = self.enabled_capabilities.clone();
            merged.try_extend_from(capabilities)?;
            Some(merged)
        } else {
            None
        };

        let Some(pending) = self.pending.take() else {
            return Err(invalid_state("IMAP tagged response without command"));
        };
        if let Some(merged) = merged_enabled {
            self.enabled_capabilities = merged;
        }
        let saved_search = self.apply_completion(
            pending.command,
            pending.saved_result_scope,
            status,
            information,
        );
        Ok(ServerSessionEvent::Completed {
            tag: pending.tag,
            command: pending.command,
            status,
            saved_search,
        })
    }

    fn apply_completion(
        &mut self,
        command: PendingCommand,
        saved_result_scope: Option<SavedSearchScope>,
        status: Status,
        information: &[u8],
    ) -> SavedSearchUpdate {
        let saved_search = match (saved_result_scope, status) {
            (Some(scope), Status::Ok) => SavedSearchUpdate::Replace(scope),
            (Some(_), Status::No) => SavedSearchUpdate::Reset,
            _ if matches!(command, PendingCommand::Select | PendingCommand::Examine)
                && status == Status::Ok =>
            {
                SavedSearchUpdate::Reset
            }
            _ => SavedSearchUpdate::Unchanged,
        };
        if self.state == SessionState::Logout {
            return saved_search;
        }
        if status == Status::Ok {
            match command {
                PendingCommand::StartTls => {
                    self.security = SecurityState::TlsHandshake;
                    self.state = SessionState::NotAuthenticated;
                }
                PendingCommand::Login | PendingCommand::Authenticate => {
                    self.state = SessionState::Authenticated;
                }
                PendingCommand::Select => {
                    self.state = SessionState::Selected {
                        read_only: information_has_code(information, b"READ-ONLY"),
                    };
                }
                PendingCommand::Examine => {
                    self.state = SessionState::Selected { read_only: true };
                }
                PendingCommand::Deselect => self.state = SessionState::Authenticated,
                PendingCommand::Logout => self.state = SessionState::Logout,
                _ => {}
            }
        } else if matches!(command, PendingCommand::Select | PendingCommand::Examine) {
            self.state = SessionState::Authenticated;
        } else if command == PendingCommand::Logout {
            self.state = SessionState::Logout;
        }
        saved_search
    }
}

impl Default for ServerSession {
    fn default() -> Self {
        Self::new(CapabilitySet::default())
    }
}

fn validate_server_command(
    command: PendingCommand,
    state: SessionState,
    security: SecurityState,
    capabilities: &CapabilitySet,
) -> Result<(), ProtocolError> {
    let allowed = match command {
        PendingCommand::Capability
        | PendingCommand::Noop
        | PendingCommand::Logout
        | PendingCommand::Extension => true,
        PendingCommand::StartTls => {
            state == SessionState::NotAuthenticated
                && security == SecurityState::Plaintext
                && capabilities.contains(&Capability::StartTls)
        }
        PendingCommand::Login | PendingCommand::Authenticate => {
            state == SessionState::NotAuthenticated
        }
        PendingCommand::Enable => {
            state == SessionState::Authenticated
                && (capabilities.contains(&Capability::Enable)
                    || capabilities.contains(&Capability::Imap4Rev2))
        }
        PendingCommand::Select
        | PendingCommand::Examine
        | PendingCommand::List
        | PendingCommand::AuthenticatedOperation => matches!(
            state,
            SessionState::Authenticated | SessionState::Selected { .. }
        ),
        PendingCommand::Deselect
        | PendingCommand::Search
        | PendingCommand::UidSearch
        | PendingCommand::SelectedOperation => {
            matches!(state, SessionState::Selected { .. })
        }
        PendingCommand::Idle => {
            matches!(
                state,
                SessionState::Authenticated | SessionState::Selected { .. }
            ) && (capabilities.contains(&Capability::Idle)
                || capabilities.contains(&Capability::Imap4Rev2))
        }
    };
    if allowed {
        Ok(())
    } else {
        Err(invalid_state(
            "IMAP server command unavailable in current state",
        ))
    }
}

fn validate_completion(
    pending: &InFlight,
    status: Status,
    bye_sent: bool,
) -> Result<(), ProtocolError> {
    let valid = match pending.continuation {
        ContinuationPhase::None
        | ContinuationPhase::AuthenticateReady
        | ContinuationPhase::IdleDone => true,
        ContinuationPhase::AuthenticateWaiting | ContinuationPhase::AuthenticateCancelled => {
            status == Status::Bad
        }
        ContinuationPhase::IdleReady => matches!(status, Status::No | Status::Bad),
        ContinuationPhase::IdleWaiting => false,
    } && (pending.command != PendingCommand::Logout
        || status == Status::Ok && bye_sent);

    if valid {
        Ok(())
    } else {
        Err(invalid_state("IMAP completion violates continuation state"))
    }
}

fn validate_enable_completion(pending: &InFlight, status: Status) -> Result<(), ProtocolError> {
    if pending.command != PendingCommand::Enable {
        return Ok(());
    }
    let valid = if status == Status::Ok {
        pending.enable_response.is_some()
    } else {
        pending.enable_response.is_none()
    };
    if valid {
        Ok(())
    } else {
        Err(invalid_state(
            "IMAP ENABLE completion disagrees with ENABLED response",
        ))
    }
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
