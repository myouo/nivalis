use bytes::Bytes;
use mail_protocol_core::wire::{eq_ascii, split_token};
use mail_protocol_core::{ErrorKind, ProtocolError};

use crate::{
    Capability, CapabilitySet, Command, CommandBody, FetchAttribute, FetchModifier,
    SavedSearchScope, SelectParameter, StatusItem,
    response::split_bracketed_response_text,
    search::{SearchMetadata, search_metadata},
    sort::{DEFAULT_SORT_MAX_DEPTH, validate_sort_arguments},
    thread::{DEFAULT_THREAD_MAX_DEPTH, validate_thread_arguments},
};

use super::{PendingCommand, SecurityState, SessionState};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SavedSearchAccess {
    None,
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CommandSemantics {
    pub(crate) command: PendingCommand,
    pub(super) exclusive: bool,
    pub(super) uses_sequence_numbers: bool,
    pub(super) blocks_sequence_numbers: bool,
    pub(super) saved_search_access: SavedSearchAccess,
    pub(crate) saved_result_scope: Option<SavedSearchScope>,
}

pub(crate) fn classify_command(command: &Command) -> CommandSemantics {
    match &command.body {
        CommandBody::Capability => semantics(PendingCommand::Capability, false, false, true),
        CommandBody::Noop => semantics(PendingCommand::Noop, false, false, true),
        CommandBody::Logout => semantics(PendingCommand::Logout, true, false, true),
        CommandBody::StartTls => semantics(PendingCommand::StartTls, true, false, true),
        CommandBody::Login { .. } => semantics(PendingCommand::Login, true, false, true),
        CommandBody::Authenticate { .. } => {
            semantics(PendingCommand::Authenticate, true, false, true)
        }
        CommandBody::Enable { .. } => semantics(PendingCommand::Enable, false, false, true),
        CommandBody::Create { .. }
        | CommandBody::Delete { .. }
        | CommandBody::Rename { .. }
        | CommandBody::Subscribe { .. }
        | CommandBody::Unsubscribe { .. }
        | CommandBody::Namespace
        | CommandBody::Status { .. }
        | CommandBody::Id { .. }
        | CommandBody::GetQuota { .. }
        | CommandBody::GetQuotaRoot { .. }
        | CommandBody::SetQuota { .. } => {
            semantics(PendingCommand::AuthenticatedOperation, false, false, true)
        }
        CommandBody::List { arguments } => semantics(
            PendingCommand::List,
            arguments.windows(2).any(|pair| pair == b"\r\n"),
            false,
            true,
        ),
        CommandBody::Lsub { arguments } | CommandBody::Append { arguments, .. } => semantics(
            PendingCommand::AuthenticatedOperation,
            arguments.windows(2).any(|pair| pair == b"\r\n"),
            false,
            true,
        ),
        CommandBody::Select { .. } | CommandBody::SelectExtended { .. } => {
            semantics(PendingCommand::Select, true, false, true)
        }
        CommandBody::Examine { .. } | CommandBody::ExamineExtended { .. } => {
            semantics(PendingCommand::Examine, true, false, true)
        }
        CommandBody::Close | CommandBody::Unselect => {
            semantics(PendingCommand::Deselect, true, false, true)
        }
        CommandBody::Idle => semantics(PendingCommand::Idle, true, false, true),
        CommandBody::Check | CommandBody::Expunge => {
            semantics(PendingCommand::SelectedOperation, false, false, true)
        }
        CommandBody::Fetch { sequence_set, .. }
        | CommandBody::Store { sequence_set, .. }
        | CommandBody::StoreConditional { sequence_set, .. } => with_saved_search_read(
            semantics(
                PendingCommand::SelectedOperation,
                false,
                !sequence_set.is_saved_search(),
                false,
            ),
            sequence_set.is_saved_search(),
        ),
        CommandBody::Copy { sequence_set, .. } | CommandBody::Move { sequence_set, .. } => {
            with_saved_search_read(
                semantics(
                    PendingCommand::SelectedOperation,
                    false,
                    !sequence_set.is_saved_search(),
                    true,
                ),
                sequence_set.is_saved_search(),
            )
        }
        CommandBody::Search { criteria } => with_search_metadata(
            semantics(
                PendingCommand::Search,
                criteria.windows(2).any(|pair| pair == b"\r\n"),
                false,
                false,
            ),
            search_metadata(criteria),
        ),
        CommandBody::Sort { arguments } => {
            classify_search_extension(arguments.as_bytes(), arguments.search_program().criteria())
        }
        CommandBody::Thread { arguments } => {
            classify_search_extension(arguments.as_bytes(), arguments.search_program().criteria())
        }
        CommandBody::Uid { command, arguments } => classify_uid_command(command, arguments),
        CommandBody::Raw { name, arguments } => classify_raw_command(name, arguments),
    }
}

fn classify_search_extension(arguments: &[u8], criteria: &[u8]) -> CommandSemantics {
    with_search_metadata(
        semantics(
            PendingCommand::SelectedOperation,
            arguments.windows(2).any(|pair| pair == b"\r\n"),
            false,
            false,
        ),
        search_metadata(criteria),
    )
}

fn classify_uid_command(command: &[u8], arguments: &[u8]) -> CommandSemantics {
    let is_search = eq_ascii(command, b"SEARCH");
    let pending = if is_search {
        PendingCommand::UidSearch
    } else {
        PendingCommand::SelectedOperation
    };
    let base = semantics(
        pending,
        arguments.windows(2).any(|pair| pair == b"\r\n"),
        false,
        true,
    );
    if is_search {
        with_search_metadata(base, search_metadata(arguments))
    } else if let Some(metadata) = uid_sort_or_thread_metadata(command, arguments) {
        with_search_metadata(base, metadata)
    } else {
        with_saved_search_read(
            base,
            uid_command_accepts_sequence_set(command) && first_token_is_saved_search(arguments),
        )
    }
}

pub(crate) fn enable_capabilities(
    command: &Command,
) -> Result<Option<CapabilitySet>, ProtocolError> {
    let tokens = match &command.body {
        CommandBody::Enable { capabilities } => {
            if capabilities.is_empty() {
                return Err(ProtocolError::new(
                    ErrorKind::InvalidSyntax,
                    "IMAP ENABLE capabilities",
                ));
            }
            capabilities.clone()
        }
        CommandBody::Raw { name, arguments } if eq_ascii(name, b"ENABLE") => {
            if arguments.is_empty()
                || arguments.first() == Some(&b' ')
                || arguments.last() == Some(&b' ')
                || arguments
                    .iter()
                    .any(|byte| matches!(byte, b'\t' | b'\r' | b'\n'))
                || arguments.windows(2).any(|pair| pair == b"  ")
            {
                return Err(ProtocolError::new(
                    ErrorKind::InvalidSyntax,
                    "IMAP ENABLE capabilities",
                ));
            }
            arguments
                .split(|byte| *byte == b' ')
                .map(Bytes::copy_from_slice)
                .collect()
        }
        _ => return Ok(None),
    };
    CapabilitySet::from_tokens(tokens).map(Some)
}

/// Enforces extension activation rules that are stricter than the base IMAP
/// authentication-state classification.
pub(crate) fn validate_extension_requirements(
    command: &Command,
    advertised: &CapabilitySet,
    enabled: &CapabilitySet,
) -> Result<(), ProtocolError> {
    if let Some(arguments) = command.parsed_select_arguments()? {
        for parameter in arguments.parameters() {
            match parameter {
                SelectParameter::QResync(_) => {
                    if !advertised.contains(&Capability::QResync)
                        || !enabled.contains(&Capability::QResync)
                    {
                        return Err(invalid_state(
                            "IMAP QRESYNC SELECT requires successful ENABLE QRESYNC",
                        ));
                    }
                }
                SelectParameter::CondStore => {
                    require_condstore(advertised, "IMAP CONDSTORE SELECT parameter")?;
                }
                SelectParameter::Other { .. } => {}
            }
        }
    }

    if let Some(arguments) = command.parsed_fetch_arguments()? {
        let has_modseq = arguments
            .attributes()
            .any(|attribute| attribute == FetchAttribute::ModSeq);
        let mut changed_since = false;
        let mut vanished = false;
        for modifier in arguments.modifiers() {
            changed_since |= matches!(modifier, FetchModifier::ChangedSince(_));
            vanished |= modifier == FetchModifier::Vanished;
        }
        if has_modseq || changed_since {
            require_condstore(advertised, "IMAP CONDSTORE FETCH data")?;
        }
        if vanished
            && (!advertised.contains(&Capability::QResync)
                || !enabled.contains(&Capability::QResync))
        {
            return Err(invalid_state(
                "IMAP VANISHED FETCH modifier requires ENABLE QRESYNC",
            ));
        }
    }

    if matches!(command.body, CommandBody::StoreConditional { .. })
        || uid_store_has_unchanged_since(command)
    {
        require_condstore(advertised, "IMAP UNCHANGEDSINCE STORE modifier")?;
    }
    validate_status_extension_requirements(command, advertised)?;
    if command.parsed_sort_arguments()?.is_some() && !advertised.contains(&Capability::Sort) {
        return Err(invalid_state("IMAP SORT capability was not advertised"));
    }
    if let Some(arguments) = command.parsed_thread_arguments()? {
        if !advertised
            .thread_algorithms()
            .any(|algorithm| eq_ascii(algorithm, arguments.algorithm().name()))
        {
            return Err(invalid_state(
                "IMAP THREAD algorithm capability was not advertised",
            ));
        }
    }
    let reads_quota = command.parsed_get_quota_arguments()?.is_some()
        || command.parsed_get_quota_root_arguments()?.is_some();
    if reads_quota && !supports_quota(advertised) {
        return Err(invalid_state("IMAP QUOTA capability was not advertised"));
    }
    if let Some(arguments) = command.parsed_set_quota_arguments()? {
        if !supports_quota(advertised) {
            return Err(invalid_state("IMAP QUOTA capability was not advertised"));
        }
        if !advertised.contains(&Capability::QuotaSet) {
            return Err(invalid_state("IMAP QUOTASET capability was not advertised"));
        }
        for limit in arguments.limits() {
            if !advertised
                .quota_resources()
                .any(|resource| eq_ascii(resource, limit.name().name()))
            {
                return Err(invalid_state(
                    "IMAP SETQUOTA resource capability was not advertised",
                ));
            }
        }
    }
    Ok(())
}

fn supports_quota(capabilities: &CapabilitySet) -> bool {
    capabilities.contains(&Capability::Quota)
}

fn validate_status_extension_requirements(
    command: &Command,
    advertised: &CapabilitySet,
) -> Result<(), ProtocolError> {
    let Some(items) = command.parsed_status_items()? else {
        return Ok(());
    };
    let mut highest_mod_sequence = false;
    let mut deleted_storage = false;
    for item in &items {
        highest_mod_sequence |= item == StatusItem::HighestModSeq;
        deleted_storage |= item == StatusItem::DeletedStorage;
    }
    if highest_mod_sequence {
        require_condstore(advertised, "IMAP HIGHESTMODSEQ STATUS item")?;
    }
    if deleted_storage
        && !advertised
            .quota_resources()
            .any(|resource| eq_ascii(resource, b"STORAGE"))
    {
        return Err(invalid_state(
            "IMAP DELETED-STORAGE STATUS item requires QUOTA=RES-STORAGE",
        ));
    }
    Ok(())
}

fn require_condstore(
    advertised: &CapabilitySet,
    context: &'static str,
) -> Result<(), ProtocolError> {
    if advertised.contains(&Capability::CondStore) || advertised.contains(&Capability::QResync) {
        Ok(())
    } else {
        Err(invalid_state(context))
    }
}

fn uid_store_has_unchanged_since(command: &Command) -> bool {
    const PREFIX: &[u8] = b"(UNCHANGEDSINCE ";
    let CommandBody::Uid { command, arguments } = &command.body else {
        return false;
    };
    if !eq_ascii(command, b"STORE") {
        return false;
    }
    let (_, after_set) = split_token(arguments);
    after_set.len() >= PREFIX.len() && eq_ascii(&after_set[..PREFIX.len()], PREFIX)
}

fn uid_sort_or_thread_metadata(command: &[u8], arguments: &[u8]) -> Option<SearchMetadata> {
    if eq_ascii(command, b"SORT") {
        validate_sort_arguments(arguments, DEFAULT_SORT_MAX_DEPTH)
            .ok()
            .map(|parsed| search_metadata(&arguments[parsed.search]))
    } else if eq_ascii(command, b"THREAD") {
        validate_thread_arguments(arguments, DEFAULT_THREAD_MAX_DEPTH)
            .ok()
            .map(|parsed| search_metadata(&arguments[parsed.search]))
    } else {
        None
    }
}

fn classify_raw_command(name: &[u8], arguments: &[u8]) -> CommandSemantics {
    let (uid_subcommand, uid_arguments) = if eq_ascii(name, b"UID") {
        split_token(arguments)
    } else {
        (&[][..], &[][..])
    };
    let kind = if eq_ascii(name, b"SEARCH") {
        PendingCommand::Search
    } else if eq_ascii(uid_subcommand, b"SEARCH") {
        PendingCommand::UidSearch
    } else {
        classify_raw(name)
    };
    let literal = arguments.windows(2).any(|pair| pair == b"\r\n");
    let exclusive = literal
        || matches!(
            kind,
            PendingCommand::Authenticate
                | PendingCommand::Idle
                | PendingCommand::StartTls
                | PendingCommand::Logout
                | PendingCommand::Select
                | PendingCommand::Examine
                | PendingCommand::Deselect
        );
    let fast_sequence = eq_ascii(name, b"FETCH")
        || eq_ascii(name, b"STORE")
        || eq_ascii(name, b"SEARCH")
        || eq_ascii(name, b"SORT")
        || eq_ascii(name, b"THREAD");
    let is_search = eq_ascii(name, b"SEARCH");
    let is_uid_search = eq_ascii(uid_subcommand, b"SEARCH");
    let search = if is_search {
        Some(search_metadata(arguments))
    } else if is_uid_search {
        Some(search_metadata(uid_arguments))
    } else {
        uid_sort_or_thread_metadata(name, arguments)
    };
    let uses_sequence_numbers = if let Some(metadata) = search {
        metadata.uses_sequence_numbers
    } else {
        eq_ascii(name, b"FETCH")
            || eq_ascii(name, b"STORE")
            || eq_ascii(name, b"COPY")
            || eq_ascii(name, b"MOVE")
    };
    let mut result = semantics(kind, exclusive, uses_sequence_numbers, !fast_sequence);
    if let Some(metadata) = search {
        result = with_search_metadata(result, metadata);
    } else {
        let saved = if eq_ascii(name, b"UID") {
            uid_command_accepts_sequence_set(uid_subcommand)
                && first_token_is_saved_search(uid_arguments)
        } else {
            command_accepts_sequence_set(name) && first_token_is_saved_search(arguments)
        };
        result = with_saved_search_read(result, saved);
        if saved && !eq_ascii(name, b"UID") {
            result.uses_sequence_numbers = false;
        }
    }
    result
}

const fn semantics(
    command: PendingCommand,
    exclusive: bool,
    uses_sequence_numbers: bool,
    blocks_sequence_numbers: bool,
) -> CommandSemantics {
    CommandSemantics {
        command,
        exclusive,
        uses_sequence_numbers,
        blocks_sequence_numbers,
        saved_search_access: SavedSearchAccess::None,
        saved_result_scope: None,
    }
}

const fn with_search_metadata(
    mut semantics: CommandSemantics,
    metadata: SearchMetadata,
) -> CommandSemantics {
    semantics.uses_sequence_numbers = metadata.uses_sequence_numbers;
    semantics.saved_result_scope = metadata.saved_result_scope;
    semantics.saved_search_access = if metadata.saved_result_scope.is_some() {
        SavedSearchAccess::Write
    } else if metadata.uses_saved_result {
        SavedSearchAccess::Read
    } else {
        SavedSearchAccess::None
    };
    semantics
}

const fn with_saved_search_read(
    mut semantics: CommandSemantics,
    uses_saved_result: bool,
) -> CommandSemantics {
    if uses_saved_result {
        semantics.saved_search_access = SavedSearchAccess::Read;
    }
    semantics
}

fn first_token_is_saved_search(arguments: &[u8]) -> bool {
    split_token(arguments).0 == b"$"
}

fn command_accepts_sequence_set(name: &[u8]) -> bool {
    eq_ascii(name, b"FETCH")
        || eq_ascii(name, b"STORE")
        || eq_ascii(name, b"COPY")
        || eq_ascii(name, b"MOVE")
}

fn uid_command_accepts_sequence_set(name: &[u8]) -> bool {
    command_accepts_sequence_set(name) || eq_ascii(name, b"EXPUNGE")
}

fn classify_raw(name: &[u8]) -> PendingCommand {
    if eq_ascii(name, b"CAPABILITY") {
        PendingCommand::Capability
    } else if eq_ascii(name, b"NOOP") {
        PendingCommand::Noop
    } else if eq_ascii(name, b"LOGOUT") {
        PendingCommand::Logout
    } else if eq_ascii(name, b"STARTTLS") {
        PendingCommand::StartTls
    } else if eq_ascii(name, b"LOGIN") {
        PendingCommand::Login
    } else if eq_ascii(name, b"AUTHENTICATE") {
        PendingCommand::Authenticate
    } else if eq_ascii(name, b"SELECT") {
        PendingCommand::Select
    } else if eq_ascii(name, b"EXAMINE") {
        PendingCommand::Examine
    } else if eq_ascii(name, b"CLOSE") || eq_ascii(name, b"UNSELECT") {
        PendingCommand::Deselect
    } else if eq_ascii(name, b"IDLE") {
        PendingCommand::Idle
    } else if eq_ascii(name, b"ENABLE") {
        PendingCommand::Enable
    } else if eq_ascii(name, b"SEARCH") {
        PendingCommand::Search
    } else if eq_ascii(name, b"LIST") {
        PendingCommand::List
    } else if is_selected_command(name) {
        PendingCommand::SelectedOperation
    } else if is_authenticated_command(name) {
        PendingCommand::AuthenticatedOperation
    } else {
        PendingCommand::Extension
    }
}

fn is_selected_command(name: &[u8]) -> bool {
    [
        b"EXPUNGE".as_slice(),
        b"SEARCH",
        b"FETCH",
        b"STORE",
        b"COPY",
        b"MOVE",
        b"UID",
        b"SORT",
        b"THREAD",
    ]
    .iter()
    .any(|candidate| eq_ascii(name, candidate))
}

fn is_authenticated_command(name: &[u8]) -> bool {
    [
        b"ENABLE".as_slice(),
        b"CREATE",
        b"DELETE",
        b"RENAME",
        b"SUBSCRIBE",
        b"UNSUBSCRIBE",
        b"LIST",
        b"LSUB",
        b"NAMESPACE",
        b"STATUS",
        b"APPEND",
        b"ID",
        b"GETQUOTA",
        b"GETQUOTAROOT",
        b"SETQUOTA",
    ]
    .iter()
    .any(|candidate| eq_ascii(name, candidate))
}

pub(super) fn validate_command_state(
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
        PendingCommand::Login => {
            state == SessionState::NotAuthenticated
                && !capabilities.contains(&Capability::LoginDisabled)
        }
        PendingCommand::Authenticate => state == SessionState::NotAuthenticated,
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
        Err(invalid_state("IMAP command unavailable in current state"))
    }
}

pub(crate) fn invalid_state(context: &'static str) -> ProtocolError {
    ProtocolError::new(ErrorKind::InvalidState, context)
}

pub(crate) fn information_has_code(information: &[u8], expected: &[u8]) -> bool {
    let Ok((code, _)) = split_bracketed_response_text(information) else {
        return false;
    };
    let (name, _) = split_token(code);
    eq_ascii(name, expected)
}
