use std::collections::HashSet;

use bytes::Bytes;
use mail_protocol_core::{ErrorKind, ProtocolError};

use crate::{
    Capability, CapabilitySet, Command, ESearchResponse, ListArguments, ListResponse, Response,
    ResponseCode, SavedSearchScope, SavedSearchUpdate, Status, StatusKind, UntaggedData,
    parse_untagged,
};

#[cfg(test)]
use crate::CommandBody;

use super::{
    ListCorrelation, PendingCommand, SecurityState, SessionEvent, SessionState,
    enable::{EnableResponse, capability_keys, enable_compatibility_graph},
    semantics::{
        SavedSearchAccess, classify_command, enable_capabilities, information_has_code,
        invalid_state, validate_command_state, validate_extension_requirements,
    },
};

const MAX_IN_FLIGHT_ENABLE: usize = 32;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct InFlight {
    tag: Bytes,
    pub(super) command: PendingCommand,
    exclusive: bool,
    blocks_sequence_numbers: bool,
    saved_search_access: SavedSearchAccess,
    saved_result_scope: Option<SavedSearchScope>,
    list_arguments: Option<ListArguments>,
    list_prefilter: u64,
    continuation: ClientContinuationPhase,
    enable_requested_keys: Option<HashSet<Bytes>>,
    pub(super) enable_request_id: Option<u64>,
    awaits_authentication: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClientContinuationPhase {
    None,
    IdleAwaitingContinuation,
    IdleActive,
    IdleDone,
}

/// Runtime-independent IMAP client protocol state machine.
///
/// The type tracks wire-visible state only. It does not perform I/O, TLS, SASL,
/// timeouts, or mailbox storage operations.
#[derive(Clone, Debug)]
pub struct ClientSession {
    state: SessionState,
    security: SecurityState,
    capabilities: CapabilitySet,
    enabled_capabilities: CapabilitySet,
    enable_responses: Vec<EnableResponse>,
    next_enable_request_id: u64,
    ever_selected: bool,
    in_flight: Vec<InFlight>,
    max_in_flight: usize,
}

impl ClientSession {
    /// Creates a session waiting for the initial server greeting.
    pub fn new(max_in_flight: usize) -> Self {
        Self {
            state: SessionState::AwaitingGreeting,
            security: SecurityState::Plaintext,
            capabilities: CapabilitySet::default(),
            enabled_capabilities: CapabilitySet::default(),
            enable_responses: Vec::new(),
            next_enable_request_id: 0,
            ever_selected: false,
            in_flight: Vec::new(),
            max_in_flight: max_in_flight.max(1),
        }
    }

    /// Creates a session whose transport is already protected by implicit TLS.
    pub fn new_tls(max_in_flight: usize) -> Self {
        let mut session = Self::new(max_in_flight);
        session.security = SecurityState::Tls;
        session
    }

    /// Returns the current authentication/mailbox state.
    pub const fn state(&self) -> SessionState {
        self.state
    }

    /// Returns the current transport protection state.
    pub const fn security(&self) -> SecurityState {
        self.security
    }

    /// Returns the most recently advertised capability set.
    pub const fn capabilities(&self) -> &CapabilitySet {
        &self.capabilities
    }

    /// Returns extensions successfully activated by completed ENABLE commands.
    pub const fn enabled_capabilities(&self) -> &CapabilitySet {
        &self.enabled_capabilities
    }

    /// Replaces cached capabilities, for example when they were carried in a
    /// response code not parsed by the basic response codec.
    pub fn set_capabilities(&mut self, capabilities: CapabilitySet) {
        self.capabilities = capabilities;
    }

    /// Returns the number of tagged commands awaiting completion.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    /// Validates and registers a command before the caller writes it to the wire.
    ///
    /// # Errors
    ///
    /// Returns an invalid-state error for duplicate tags, commands unavailable in
    /// the current state, unsupported STARTTLS/IDLE usage, a malformed manually
    /// constructed LIST, or a full/exclusive in-flight window.
    pub fn register_command(&mut self, command: &Command) -> Result<PendingCommand, ProtocolError> {
        if self.state == SessionState::AwaitingGreeting || self.state == SessionState::Logout {
            return Err(invalid_state(
                "IMAP command before greeting or after logout",
            ));
        }
        if self.security == SecurityState::TlsHandshake {
            return Err(invalid_state("IMAP command during TLS handshake"));
        }
        if self
            .in_flight
            .iter()
            .any(|pending| pending.tag == command.tag)
        {
            return Err(invalid_state("duplicate in-flight IMAP tag"));
        }
        if self.in_flight.len() >= self.max_in_flight {
            return Err(invalid_state("IMAP in-flight command limit"));
        }

        let semantics = classify_command(command);
        if semantics.command == PendingCommand::Enable
            && self
                .in_flight
                .iter()
                .filter(|pending| pending.command == PendingCommand::Enable)
                .count()
                >= MAX_IN_FLIGHT_ENABLE
        {
            return Err(invalid_state("IMAP in-flight ENABLE limit"));
        }
        let enable_requested = enable_capabilities(command)?;
        let enable_requested_keys = enable_requested.as_ref().map(capability_keys).transpose()?;
        let list_arguments = command.parsed_list_arguments()?;
        let list_prefilter = list_arguments
            .as_ref()
            .map_or(0, ListArguments::child_info_prefilter);
        if semantics.command == PendingCommand::Enable && self.ever_selected {
            return Err(invalid_state(
                "IMAP ENABLE after this client selected a mailbox",
            ));
        }
        let registration_state = self.projected_registration_state(semantics.command);
        let awaits_authentication = self.state == SessionState::NotAuthenticated
            && registration_state == SessionState::Authenticated;
        validate_command_state(
            semantics.command,
            registration_state,
            self.security,
            &self.capabilities,
        )?;
        validate_extension_requirements(command, &self.capabilities, &self.enabled_capabilities)?;
        let overlaps_exclusive = semantics.exclusive && !self.in_flight.is_empty()
            || self.in_flight.iter().any(|pending| pending.exclusive);
        if overlaps_exclusive && !self.allows_enable_pipeline_overlap(semantics.command) {
            return Err(invalid_state("exclusive IMAP command overlap"));
        }
        if semantics.uses_sequence_numbers
            && self
                .in_flight
                .iter()
                .any(|pending| pending.blocks_sequence_numbers)
        {
            return Err(invalid_state("ambiguous pipelined IMAP sequence numbers"));
        }

        let enable_request_id = if enable_requested_keys.is_some() {
            let request_id = self.next_enable_request_id;
            self.next_enable_request_id = request_id.checked_add(1).ok_or_else(|| {
                ProtocolError::new(
                    ErrorKind::FrameTooLarge,
                    "IMAP ENABLE request identity space",
                )
            })?;
            Some(request_id)
        } else {
            None
        };

        self.in_flight.push(InFlight {
            tag: command.tag.clone(),
            command: semantics.command,
            exclusive: semantics.exclusive,
            blocks_sequence_numbers: semantics.blocks_sequence_numbers,
            saved_search_access: semantics.saved_search_access,
            saved_result_scope: semantics.saved_result_scope,
            list_arguments,
            list_prefilter,
            continuation: if semantics.command == PendingCommand::Idle {
                ClientContinuationPhase::IdleAwaitingContinuation
            } else {
                ClientContinuationPhase::None
            },
            enable_requested_keys,
            enable_request_id,
            awaits_authentication,
        });
        if matches!(
            semantics.command,
            PendingCommand::Select | PendingCommand::Examine
        ) {
            self.ever_selected = true;
        }
        Ok(semantics.command)
    }

    fn projected_registration_state(&self, command: PendingCommand) -> SessionState {
        if self.state == SessionState::NotAuthenticated
            && matches!(
                command,
                PendingCommand::Enable | PendingCommand::Select | PendingCommand::Examine
            )
            && self.login_enable_pipeline_prefix()
            && (command == PendingCommand::Enable
                || self
                    .in_flight
                    .iter()
                    .skip(1)
                    .any(|pending| pending.command == PendingCommand::Enable))
        {
            SessionState::Authenticated
        } else {
            self.state
        }
    }

    fn allows_enable_pipeline_overlap(&self, command: PendingCommand) -> bool {
        match command {
            PendingCommand::Enable => self.login_enable_pipeline_prefix(),
            PendingCommand::Select | PendingCommand::Examine => {
                (!self.in_flight.is_empty()
                    && self
                        .in_flight
                        .iter()
                        .all(|pending| pending.command == PendingCommand::Enable))
                    || (self.login_enable_pipeline_prefix()
                        && self
                            .in_flight
                            .iter()
                            .skip(1)
                            .any(|pending| pending.command == PendingCommand::Enable))
            }
            _ => false,
        }
    }

    fn login_enable_pipeline_prefix(&self) -> bool {
        self.state == SessionState::NotAuthenticated
            && self.in_flight.first().is_some_and(|pending| {
                pending.command == PendingCommand::Login
                    && self
                        .in_flight
                        .iter()
                        .skip(1)
                        .all(|later| later.command == PendingCommand::Enable)
            })
    }

    /// Applies one decoded server response.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid greeting, unmatched tagged response,
    /// unexpected continuation, or malformed CAPABILITY data.
    pub fn on_response(&mut self, response: &Response) -> Result<SessionEvent, ProtocolError> {
        if self.state == SessionState::AwaitingGreeting {
            return self.on_greeting(response);
        }
        if self.security == SecurityState::TlsHandshake {
            return Err(invalid_state("IMAP response during TLS handshake"));
        }

        match response {
            Response::Continuation { data } => {
                let Some(pending) = self.in_flight.iter_mut().find(|pending| pending.exclusive)
                else {
                    return Err(invalid_state("unexpected IMAP continuation"));
                };
                match pending.command {
                    PendingCommand::Authenticate => {}
                    PendingCommand::Idle
                        if pending.continuation
                            == ClientContinuationPhase::IdleAwaitingContinuation =>
                    {
                        pending.continuation = ClientContinuationPhase::IdleActive;
                    }
                    _ => return Err(invalid_state("unexpected IMAP continuation")),
                }
                Ok(SessionEvent::Continuation {
                    command: pending.command,
                    data: data.clone(),
                })
            }
            Response::Untagged { .. } => match parse_untagged(response)? {
                Some(UntaggedData::Capability(capabilities)) => {
                    self.capabilities = capabilities;
                    Ok(SessionEvent::CapabilitiesUpdated)
                }
                Some(UntaggedData::Enabled(capabilities)) => self.on_enabled(capabilities),
                Some(UntaggedData::Status(status)) => {
                    if status.kind == StatusKind::Bye {
                        self.state = SessionState::Logout;
                        return Ok(SessionEvent::ServerBye);
                    }
                    if let Some(ResponseCode::Capability(capabilities)) = &status.code {
                        self.capabilities = capabilities.clone();
                        return Ok(SessionEvent::CapabilitiesUpdated);
                    }
                    if status.code == Some(ResponseCode::Closed) {
                        if status.kind != StatusKind::Ok
                            || !matches!(self.state, SessionState::Selected { .. })
                            || !self.in_flight.iter().any(|pending| {
                                matches!(
                                    pending.command,
                                    PendingCommand::Select | PendingCommand::Examine
                                )
                            })
                        {
                            return Err(invalid_state("unexpected IMAP CLOSED response code"));
                        }
                        self.state = SessionState::Authenticated;
                        return Ok(SessionEvent::MailboxClosed);
                    }
                    if matches!(status.code, Some(ResponseCode::UidValidity(_)))
                        && matches!(self.state, SessionState::Selected { .. })
                        && !self.in_flight.iter().any(|pending| {
                            matches!(
                                pending.command,
                                PendingCommand::Select | PendingCommand::Examine
                            )
                        })
                    {
                        return Ok(SessionEvent::SavedSearchReset);
                    }
                    Ok(SessionEvent::Unsolicited)
                }
                Some(UntaggedData::ESearch(result)) => self.on_esearch(result),
                Some(UntaggedData::List(result)) => self.on_list(result),
                Some(UntaggedData::Vanished { .. })
                    if !self.enabled_capabilities.contains(&Capability::QResync) =>
                {
                    Err(invalid_state("IMAP VANISHED before ENABLE QRESYNC"))
                }
                _ => Ok(SessionEvent::Unsolicited),
            },
            Response::Tagged {
                tag,
                status,
                information,
            } => self.on_completion(tag, *status, information),
        }
    }

    /// Records that the client sent the decoded `DONE` continuation for IDLE.
    ///
    /// Untagged responses remain valid after this call until the tagged IDLE
    /// completion arrives. The caller remains responsible for the 29-minute
    /// renewal timer represented by [`crate::IDLE_REISSUE_INTERVAL`].
    ///
    /// # Errors
    ///
    /// Returns an invalid-state error unless an IDLE command has received its
    /// continuation request and has not already sent `DONE`.
    pub fn on_idle_done(&mut self, _: crate::IdleDone) -> Result<(), ProtocolError> {
        let Some(pending) = self
            .in_flight
            .iter_mut()
            .find(|pending| pending.command == PendingCommand::Idle)
        else {
            return Err(invalid_state("IMAP IDLE DONE without command"));
        };
        if pending.continuation != ClientContinuationPhase::IdleActive {
            return Err(invalid_state("unexpected IMAP IDLE DONE"));
        }
        pending.continuation = ClientContinuationPhase::IdleDone;
        Ok(())
    }

    /// Confirms that the external TLS handshake completed successfully.
    ///
    /// # Errors
    ///
    /// Returns an invalid-state error unless STARTTLS has received tagged OK.
    pub fn on_tls_established(&mut self) -> Result<(), ProtocolError> {
        if self.security != SecurityState::TlsHandshake || !self.in_flight.is_empty() {
            return Err(invalid_state("IMAP TLS established outside handshake"));
        }
        self.security = SecurityState::Tls;
        self.state = SessionState::NotAuthenticated;
        self.capabilities = CapabilitySet::default();
        self.enabled_capabilities = CapabilitySet::default();
        Ok(())
    }

    /// Marks the external TLS handshake as failed and makes the session unusable.
    ///
    /// # Errors
    ///
    /// Returns an invalid-state error unless STARTTLS has received tagged OK.
    pub fn on_tls_failed(&mut self) -> Result<(), ProtocolError> {
        if self.security != SecurityState::TlsHandshake || !self.in_flight.is_empty() {
            return Err(invalid_state("IMAP TLS failure outside handshake"));
        }
        self.state = SessionState::Logout;
        Ok(())
    }

    fn on_greeting(&mut self, response: &Response) -> Result<SessionEvent, ProtocolError> {
        let Some(UntaggedData::Status(status)) = parse_untagged(response)? else {
            return Err(invalid_state("invalid IMAP greeting response kind"));
        };
        self.state = if status.kind == StatusKind::Ok {
            SessionState::NotAuthenticated
        } else if status.kind == StatusKind::Preauth {
            SessionState::Authenticated
        } else if status.kind == StatusKind::Bye {
            SessionState::Logout
        } else {
            return Err(ProtocolError::new(
                ErrorKind::InvalidSyntax,
                "IMAP server greeting",
            ));
        };
        if let Some(ResponseCode::Capability(capabilities)) = status.code {
            self.capabilities = capabilities;
        }
        Ok(SessionEvent::Greeting { state: self.state })
    }

    fn on_esearch(&self, result: ESearchResponse) -> Result<SessionEvent, ProtocolError> {
        if let Some(tag) = result.tag() {
            let tag = tag.decoded();
            let Some(pending) = self
                .in_flight
                .iter()
                .find(|pending| pending.tag.as_ref() == tag.as_ref())
            else {
                return Err(invalid_state("unmatched IMAP ESEARCH correlator"));
            };
            let valid = matches!(
                (pending.command, result.is_uid()),
                (PendingCommand::Search, false) | (PendingCommand::UidSearch, true)
            );
            if !valid {
                return Err(invalid_state("mismatched IMAP ESEARCH result kind"));
            }
        }
        Ok(SessionEvent::SearchResults { response: result })
    }

    fn on_enabled(&mut self, capabilities: CapabilitySet) -> Result<SessionEvent, ProtocolError> {
        if !self
            .in_flight
            .iter()
            .any(|pending| pending.command == PendingCommand::Enable)
        {
            return Err(invalid_state("IMAP ENABLED without pending ENABLE"));
        }
        if capabilities
            .iter()
            .any(|capability| !self.capabilities.contains(capability))
        {
            return Err(invalid_state("IMAP ENABLED capability was not advertised"));
        }
        let keys = capability_keys(&capabilities)?;
        let mut compatible_requests = HashSet::new();
        for pending in self
            .in_flight
            .iter()
            .filter(|pending| pending.command == PendingCommand::Enable)
        {
            let (Some(requested), Some(request_id)) = (
                pending.enable_requested_keys.as_ref(),
                pending.enable_request_id,
            ) else {
                return Err(invalid_state(
                    "IMAP ENABLE command lost requested capabilities",
                ));
            };
            if keys.is_subset(requested) {
                compatible_requests.insert(request_id);
            }
        }
        let response = EnableResponse {
            capabilities: capabilities.clone(),
            compatible_requests,
        };
        if !enable_compatibility_graph(
            &self.in_flight,
            self.enable_responses
                .iter()
                .chain(core::iter::once(&response)),
        )?
        .has_matching(None, None)
        {
            return Err(invalid_state(
                "IMAP ENABLED capability was not requested or was duplicated",
            ));
        }
        self.enable_responses.push(response);
        Ok(SessionEvent::CapabilitiesEnabled { capabilities })
    }

    fn on_completion(
        &mut self,
        tag: &Bytes,
        status: Status,
        information: &[u8],
    ) -> Result<SessionEvent, ProtocolError> {
        let Some(index) = self
            .in_flight
            .iter()
            .position(|pending| pending.tag == *tag)
        else {
            return Err(invalid_state("unmatched IMAP tagged response"));
        };
        if !self.saved_search_completion_allowed(index) {
            return Err(invalid_state(
                "out-of-order IMAP saved-search dependency completion",
            ));
        }
        validate_client_completion(&self.in_flight[index], status)?;
        validate_projected_completion(&self.in_flight[index], self.state, status)?;
        let enable_response_index = self.enable_completion_response(index, status)?;
        let merged_enabled = if let Some(response_index) = enable_response_index {
            let mut merged = self.enabled_capabilities.clone();
            merged.try_extend_from(&self.enable_responses[response_index].capabilities)?;
            Some(merged)
        } else {
            None
        };
        if enable_response_index.is_some() != merged_enabled.is_some() {
            return Err(invalid_state("IMAP ENABLE capability merge lost state"));
        }
        let pending = self.in_flight.remove(index);
        if let Some((response_index, merged)) = enable_response_index.zip(merged_enabled) {
            self.enable_responses.remove(response_index);
            self.enabled_capabilities = merged;
        }
        let saved_search = self.apply_completion(
            pending.command,
            pending.saved_result_scope,
            status,
            information,
        );
        Ok(SessionEvent::Completed {
            tag: tag.clone(),
            command: pending.command,
            status,
            saved_search,
        })
    }

    fn enable_completion_response(
        &self,
        pending_index: usize,
        status: Status,
    ) -> Result<Option<usize>, ProtocolError> {
        let pending = &self.in_flight[pending_index];
        if pending.command != PendingCommand::Enable {
            return Ok(None);
        }

        let graph = enable_compatibility_graph(&self.in_flight, &self.enable_responses)?;
        let request_index = self.in_flight[..pending_index]
            .iter()
            .filter(|pending| pending.command == PendingCommand::Enable)
            .count();
        if request_index >= graph.request_count {
            return Err(invalid_state(
                "IMAP ENABLE command lost requested capabilities",
            ));
        }
        if status != Status::Ok {
            if graph.has_matching(Some(request_index), None) {
                return Ok(None);
            }
            return Err(invalid_state(
                "IMAP failed ENABLE conflicts with an ENABLED response",
            ));
        }

        for response_index in 0..self.enable_responses.len() {
            if !graph.edges[response_index][request_index] {
                continue;
            }
            if graph.has_matching(Some(request_index), Some(response_index)) {
                return Ok(Some(response_index));
            }
        }
        Err(invalid_state(
            "IMAP ENABLE success without a corresponding ENABLED response",
        ))
    }

    fn on_list(&self, response: ListResponse) -> Result<SessionEvent, ProtocolError> {
        let Some(response_prefilter) = response.child_info_prefilter() else {
            return Ok(SessionEvent::ListData {
                response,
                correlation: ListCorrelation::Unspecified,
            });
        };

        let mut matched_tag = None;
        let mut ambiguous = false;
        for pending in &self.in_flight {
            let Some(arguments) = pending.list_arguments.as_ref() else {
                continue;
            };
            if response_prefilter & !pending.list_prefilter != 0 {
                continue;
            }
            if !arguments.correlates_child_info(&response) {
                continue;
            }
            if matched_tag.is_some() {
                ambiguous = true;
                break;
            }
            matched_tag = Some(pending.tag.clone());
        }
        let correlation = if ambiguous {
            ListCorrelation::Ambiguous
        } else if let Some(tag) = matched_tag {
            ListCorrelation::Matched { tag }
        } else {
            return Err(invalid_state(
                "IMAP CHILDINFO without matching LIST command",
            ));
        };
        Ok(SessionEvent::ListData {
            response,
            correlation,
        })
    }

    fn saved_search_completion_allowed(&self, index: usize) -> bool {
        let access = self.in_flight[index].saved_search_access;
        match access {
            SavedSearchAccess::None => true,
            SavedSearchAccess::Read => !self.in_flight[..index]
                .iter()
                .any(|pending| pending.saved_search_access == SavedSearchAccess::Write),
            SavedSearchAccess::Write => self.in_flight[..index]
                .iter()
                .all(|pending| pending.saved_search_access == SavedSearchAccess::None),
        }
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
        if status == Status::Ok {
            match command {
                PendingCommand::StartTls => {
                    self.security = SecurityState::TlsHandshake;
                    self.capabilities = CapabilitySet::default();
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
        } else if matches!(command, PendingCommand::Select | PendingCommand::Examine)
            && self.state != SessionState::NotAuthenticated
        {
            self.state = SessionState::Authenticated;
        }
        saved_search
    }
}

fn validate_client_completion(pending: &InFlight, status: Status) -> Result<(), ProtocolError> {
    let valid = match pending.continuation {
        ClientContinuationPhase::None | ClientContinuationPhase::IdleDone => true,
        ClientContinuationPhase::IdleAwaitingContinuation => {
            matches!(status, Status::No | Status::Bad)
        }
        ClientContinuationPhase::IdleActive => false,
    };
    if valid {
        Ok(())
    } else {
        Err(invalid_state("IMAP completion violates continuation state"))
    }
}

fn validate_projected_completion(
    pending: &InFlight,
    state: SessionState,
    status: Status,
) -> Result<(), ProtocolError> {
    if status == Status::Ok
        && pending.awaits_authentication
        && state == SessionState::NotAuthenticated
    {
        Err(invalid_state(
            "IMAP projected command succeeded before authentication",
        ))
    } else {
        Ok(())
    }
}

impl Default for ClientSession {
    fn default() -> Self {
        Self::new(32)
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
