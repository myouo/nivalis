use super::{
    BufMut, Bytes, BytesMut, Command, CommandBody, CommandBodyRef, CommandKind, CommandRef,
    ErrorKind, FrameMode, FrameScanner, Limits, ProtocolError, Response, SelectArguments,
    SequenceSetRef, StoreOperation, classify_command, eq_ascii, literal_spec, memchr2,
    parse_astring_prefix, parse_response, slice_for, split_sequence_and_items, split_token,
    split_token_preserve_tail, validate_append_arguments, validate_astring, validate_atom,
    validate_fetch_arguments, validate_id_command, validate_initial_response,
    validate_list_arguments, validate_raw, validate_search_program, validate_select_arguments,
    validate_status_items, validate_store_flags, validate_tag, validate_uid_fetch_arguments,
};
use crate::{
    GetQuotaArguments, GetQuotaRootArguments, SetQuotaArguments, SortArguments, ThreadArguments,
    quota::{
        validate_get_quota_arguments, validate_get_quota_root_arguments,
        validate_set_quota_arguments,
    },
    sort::validate_sort_arguments,
    thread::validate_thread_arguments,
};

impl<'a> CommandRef<'a> {
    /// Parses exactly one complete IMAP command frame without allocating.
    ///
    /// This entry point validates literal-aware framing and the same command
    /// semantics as [`Command::parse`].
    ///
    /// # Errors
    ///
    /// Returns an error for incomplete or trailing frames, invalid literal
    /// framing, exceeded limits, or invalid command syntax and semantics.
    pub fn parse(wire: &'a [u8]) -> Result<Self, ProtocolError> {
        Self::parse_with_limits(wire, Limits::default())
    }

    /// Parses one borrowed complete frame with explicit resource limits.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::parse`] using `limits` for framing,
    /// literal size, and recursive syntax budgets.
    pub fn parse_with_limits(wire: &'a [u8], limits: Limits) -> Result<Self, ProtocolError> {
        validate_exact_command_frame(wire, limits)?;
        parse_command_ref(wire, limits)
    }
}

impl Command {
    /// Parses and owns exactly one complete IMAP command frame without copying
    /// its field or literal bytes.
    ///
    /// This entry point is intended for callers that already own a complete
    /// [`Bytes`] frame. Use [`crate::CommandDecoder`] for incrementally received data
    /// and [`crate::ServerCommandDecoder`] when synchronizing literal continuation
    /// must be enforced.
    ///
    /// # Errors
    ///
    /// Returns an error for incomplete or trailing frames, invalid literal
    /// framing, exceeded limits, or invalid command syntax and semantics.
    pub fn parse(wire: Bytes) -> Result<Self, ProtocolError> {
        Self::parse_with_limits(wire, Limits::default())
    }

    /// Parses one owned complete frame with explicit resource limits.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::parse`] using `limits` for framing,
    /// literal size, and recursive syntax budgets.
    pub fn parse_with_limits(wire: Bytes, limits: Limits) -> Result<Self, ProtocolError> {
        let borrowed = CommandRef::parse_with_limits(&wire, limits)?;
        let command = own_command(&wire, borrowed);
        drop(wire);
        Ok(command)
    }
}

fn validate_exact_command_frame(wire: &[u8], limits: Limits) -> Result<(), ProtocolError> {
    if wire
        .strip_suffix(b"\r\n")
        .is_some_and(|content| memchr2(b'\r', b'\n', content).is_none())
    {
        let content = &wire[..wire.len() - 2];
        if content.len() > limits.max_line_len() {
            return Err(ProtocolError::new(ErrorKind::LineTooLong, "IMAP line").at(0));
        }
        if wire.len() > limits.max_frame_len() {
            return Err(ProtocolError::new(ErrorKind::FrameTooLarge, "IMAP frame"));
        }
        if literal_spec(content)?.is_none() {
            return Ok(());
        }
    }

    let mut scanner = FrameScanner::default();
    let end = scanner.frame_end(
        wire,
        limits,
        FrameMode::Command {
            literal_plus: false,
            require_continuation: false,
        },
    )?;
    if end == Some(wire.len()) {
        Ok(())
    } else {
        Err(ProtocolError::new(
            ErrorKind::InvalidSyntax,
            "IMAP command frame",
        ))
    }
}

impl Response {
    /// Parses and owns exactly one complete IMAP response frame without
    /// copying its field or literal bytes.
    ///
    /// This entry point is intended for callers that already own a complete
    /// [`Bytes`] frame. Use [`crate::ResponseDecoder`] for incrementally received
    /// data. Both paths validate recognized typed response semantics before
    /// consuming or returning the frame.
    ///
    /// # Errors
    ///
    /// Returns an error for incomplete or trailing frames, invalid literal
    /// framing, exceeded limits, or invalid top-level response syntax.
    pub fn parse(wire: Bytes) -> Result<Self, ProtocolError> {
        Self::parse_with_limits(wire, Limits::default())
    }

    /// Parses one owned complete response frame with explicit resource
    /// limits.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::parse`] using `limits` for framing
    /// and literal-size budgets.
    pub fn parse_with_limits(wire: Bytes, limits: Limits) -> Result<Self, ProtocolError> {
        let mut scanner = FrameScanner::default();
        let end = scanner.frame_end(&wire, limits, FrameMode::Response)?;
        if end != Some(wire.len()) {
            return Err(ProtocolError::new(
                ErrorKind::InvalidSyntax,
                "IMAP response frame",
            ));
        }
        let response = parse_response(&wire);
        drop(wire);
        response
    }
}

pub(super) fn parse_command(frame: &Bytes, limits: Limits) -> Result<Command, ProtocolError> {
    let command = parse_command_ref(frame, limits)?;
    Ok(own_command(frame, command))
}

#[allow(clippy::too_many_lines)]
fn parse_command_ref(frame: &[u8], limits: Limits) -> Result<CommandRef<'_>, ProtocolError> {
    let content = &frame[..frame.len() - 2];
    let (tag_bytes, after_tag) = split_required_space(content, "IMAP command tag separator")?;
    validate_tag(tag_bytes)?;
    let (name_bytes, arguments) = split_optional_arguments(after_tag)?;
    let kind = classify_command(name_bytes);
    if kind == CommandKind::Raw {
        validate_atom(name_bytes, "IMAP command name")?;
    }

    let body = if kind == CommandKind::Capability {
        no_ref_args(arguments, CommandBodyRef::Capability)?
    } else if kind == CommandKind::Noop {
        no_ref_args(arguments, CommandBodyRef::Noop)?
    } else if kind == CommandKind::Logout {
        no_ref_args(arguments, CommandBodyRef::Logout)?
    } else if kind == CommandKind::StartTls {
        no_ref_args(arguments, CommandBodyRef::StartTls)?
    } else if kind == CommandKind::Idle {
        no_ref_args(arguments, CommandBodyRef::Idle)?
    } else if kind == CommandKind::Check {
        no_ref_args(arguments, CommandBodyRef::Check)?
    } else if kind == CommandKind::Close {
        no_ref_args(arguments, CommandBodyRef::Close)?
    } else if kind == CommandKind::Expunge {
        no_ref_args(arguments, CommandBodyRef::Expunge)?
    } else if kind == CommandKind::Unselect {
        no_ref_args(arguments, CommandBodyRef::Unselect)?
    } else if kind == CommandKind::Namespace {
        no_ref_args(arguments, CommandBodyRef::Namespace)?
    } else if kind == CommandKind::Select {
        let parsed = validate_select_arguments(arguments, limits.max_nesting_depth())?;
        if parsed.parameters.is_some() {
            CommandBodyRef::SelectExtended { arguments }
        } else {
            CommandBodyRef::Select {
                mailbox: &arguments[..parsed.mailbox_end],
            }
        }
    } else if kind == CommandKind::Examine {
        let parsed = validate_select_arguments(arguments, limits.max_nesting_depth())?;
        if parsed.parameters.is_some() {
            CommandBodyRef::ExamineExtended { arguments }
        } else {
            CommandBodyRef::Examine {
                mailbox: &arguments[..parsed.mailbox_end],
            }
        }
    } else if kind == CommandKind::Login {
        let (username, remaining) = take_astring(arguments)?;
        let password_input = take_required_tail(remaining, "IMAP LOGIN separator")?;
        let (password, tail) = take_astring(password_input)?;
        if !tail.is_empty() {
            return Err(ProtocolError::new(
                ErrorKind::InvalidSyntax,
                "IMAP LOGIN arguments",
            ));
        }
        CommandBodyRef::Login { username, password }
    } else if kind == CommandKind::Authenticate && !arguments.windows(2).any(|pair| pair == b"\r\n")
    {
        let (mechanism, remaining) = split_token(arguments);
        validate_atom(mechanism, "IMAP AUTHENTICATE mechanism")?;
        let initial_response = if remaining.is_empty() {
            None
        } else {
            let (response, tail) = split_token(remaining);
            if !tail.is_empty() {
                return Err(ProtocolError::new(
                    ErrorKind::InvalidSyntax,
                    "IMAP AUTHENTICATE arguments",
                ));
            }
            validate_initial_response(response)?;
            Some(response)
        };
        CommandBodyRef::Authenticate {
            mechanism,
            initial_response,
        }
    } else if kind == CommandKind::Enable && !arguments.windows(2).any(|pair| pair == b"\r\n") {
        validate_atom_list(arguments, "IMAP ENABLE capabilities")?;
        CommandBodyRef::Enable {
            capabilities: arguments,
        }
    } else if kind == CommandKind::Create {
        CommandBodyRef::Create {
            mailbox: one_ref_argument(arguments, "IMAP CREATE")?,
        }
    } else if kind == CommandKind::Delete {
        CommandBodyRef::Delete {
            mailbox: one_ref_argument(arguments, "IMAP DELETE")?,
        }
    } else if kind == CommandKind::Rename {
        let (from, to) = two_ref_astrings(arguments, "IMAP RENAME")?;
        CommandBodyRef::Rename { from, to }
    } else if kind == CommandKind::Subscribe {
        CommandBodyRef::Subscribe {
            mailbox: one_ref_argument(arguments, "IMAP SUBSCRIBE")?,
        }
    } else if kind == CommandKind::Unsubscribe {
        CommandBodyRef::Unsubscribe {
            mailbox: one_ref_argument(arguments, "IMAP UNSUBSCRIBE")?,
        }
    } else if kind == CommandKind::List {
        validate_list_arguments(arguments, limits.max_nesting_depth())?;
        CommandBodyRef::List { arguments }
    } else if kind == CommandKind::Lsub {
        required_raw(arguments, "IMAP LSUB arguments")?;
        CommandBodyRef::Lsub { arguments }
    } else if kind == CommandKind::Status {
        let (mailbox, remaining) = take_astring(arguments)?;
        let items = take_required_tail(remaining, "IMAP STATUS separator")?;
        validate_status_items(items)?;
        CommandBodyRef::Status { mailbox, items }
    } else if kind == CommandKind::Append {
        let (mailbox, remaining) = take_astring(arguments)?;
        let append_arguments = take_required_tail(remaining, "IMAP APPEND separator")?;
        validate_append_arguments(append_arguments)?;
        CommandBodyRef::Append {
            mailbox,
            arguments: append_arguments,
        }
    } else if kind == CommandKind::Id {
        validate_id_command(arguments)?;
        CommandBodyRef::Id {
            parameters: arguments,
        }
    } else if kind == CommandKind::Search {
        validate_search_program(arguments, limits.max_nesting_depth())?;
        CommandBodyRef::Search {
            criteria: arguments,
        }
    } else if kind == CommandKind::Fetch {
        let (sequence, item_input) = split_sequence_and_items(arguments)?;
        let sequence_set = SequenceSetRef::parse(sequence)?;
        validate_fetch_arguments(item_input, limits.max_nesting_depth(), false)?;
        CommandBodyRef::Fetch {
            sequence_set,
            items: item_input,
        }
    } else if kind == CommandKind::Store {
        parse_store_ref(arguments)?
    } else if kind == CommandKind::Copy {
        let (sequence_set, mailbox) = sequence_and_mailbox_ref(arguments, "IMAP COPY")?;
        CommandBodyRef::Copy {
            sequence_set,
            mailbox,
        }
    } else if kind == CommandKind::Move {
        let (sequence_set, mailbox) = sequence_and_mailbox_ref(arguments, "IMAP MOVE")?;
        CommandBodyRef::Move {
            sequence_set,
            mailbox,
        }
    } else if kind == CommandKind::Uid {
        let (command, remaining) = split_token_preserve_tail(arguments);
        validate_atom(command, "IMAP UID subcommand")?;
        required_raw(remaining, "IMAP UID arguments")?;
        validate_uid_arguments(command, remaining, limits.max_nesting_depth())?;
        CommandBodyRef::Uid {
            command,
            arguments: remaining,
        }
    } else if kind == CommandKind::GetQuota {
        validate_get_quota_arguments(arguments)?;
        CommandBodyRef::GetQuota { arguments }
    } else if kind == CommandKind::GetQuotaRoot {
        validate_get_quota_root_arguments(arguments)?;
        CommandBodyRef::GetQuotaRoot { arguments }
    } else if kind == CommandKind::SetQuota {
        validate_set_quota_arguments(arguments)?;
        CommandBodyRef::SetQuota { arguments }
    } else if kind == CommandKind::Sort {
        validate_sort_arguments(arguments, limits.max_nesting_depth())?;
        CommandBodyRef::Sort { arguments }
    } else if kind == CommandKind::Thread {
        validate_thread_arguments(arguments, limits.max_nesting_depth())?;
        CommandBodyRef::Thread { arguments }
    } else {
        CommandBodyRef::Raw {
            name: name_bytes,
            arguments,
        }
    };
    Ok(CommandRef {
        tag: tag_bytes,
        body,
    })
}

#[allow(clippy::too_many_lines)]
pub(crate) fn own_command(frame: &Bytes, command: CommandRef<'_>) -> Command {
    let body = match command.body {
        CommandBodyRef::Capability => CommandBody::Capability,
        CommandBodyRef::Noop => CommandBody::Noop,
        CommandBodyRef::Logout => CommandBody::Logout,
        CommandBodyRef::StartTls => CommandBody::StartTls,
        CommandBodyRef::Idle => CommandBody::Idle,
        CommandBodyRef::Check => CommandBody::Check,
        CommandBodyRef::Close => CommandBody::Close,
        CommandBodyRef::Expunge => CommandBody::Expunge,
        CommandBodyRef::Login { username, password } => CommandBody::Login {
            username: slice_for(frame, username),
            password: slice_for(frame, password),
        },
        CommandBodyRef::Authenticate {
            mechanism,
            initial_response,
        } => CommandBody::Authenticate {
            mechanism: slice_for(frame, mechanism),
            initial_response: initial_response.map(|value| slice_for(frame, value)),
        },
        CommandBodyRef::Enable { capabilities } => CommandBody::Enable {
            capabilities: own_atom_list(frame, capabilities),
        },
        CommandBodyRef::Select { mailbox } => CommandBody::Select {
            mailbox: slice_for(frame, mailbox),
        },
        CommandBodyRef::SelectExtended { arguments } => {
            let arguments = slice_for(frame, arguments);
            CommandBody::SelectExtended {
                arguments: SelectArguments::parse_with_max_depth(&arguments, usize::MAX)
                    .expect("borrowed SELECT arguments were already validated"),
            }
        }
        CommandBodyRef::Examine { mailbox } => CommandBody::Examine {
            mailbox: slice_for(frame, mailbox),
        },
        CommandBodyRef::ExamineExtended { arguments } => {
            let arguments = slice_for(frame, arguments);
            CommandBody::ExamineExtended {
                arguments: SelectArguments::parse_with_max_depth(&arguments, usize::MAX)
                    .expect("borrowed EXAMINE arguments were already validated"),
            }
        }
        CommandBodyRef::Unselect => CommandBody::Unselect,
        CommandBodyRef::Create { mailbox } => CommandBody::Create {
            mailbox: slice_for(frame, mailbox),
        },
        CommandBodyRef::Delete { mailbox } => CommandBody::Delete {
            mailbox: slice_for(frame, mailbox),
        },
        CommandBodyRef::Rename { from, to } => CommandBody::Rename {
            from: slice_for(frame, from),
            to: slice_for(frame, to),
        },
        CommandBodyRef::Subscribe { mailbox } => CommandBody::Subscribe {
            mailbox: slice_for(frame, mailbox),
        },
        CommandBodyRef::Unsubscribe { mailbox } => CommandBody::Unsubscribe {
            mailbox: slice_for(frame, mailbox),
        },
        CommandBodyRef::List { arguments } => CommandBody::List {
            arguments: slice_for(frame, arguments),
        },
        CommandBodyRef::Lsub { arguments } => CommandBody::Lsub {
            arguments: slice_for(frame, arguments),
        },
        CommandBodyRef::Namespace => CommandBody::Namespace,
        CommandBodyRef::GetQuota { arguments } => {
            let arguments = slice_for(frame, arguments);
            CommandBody::GetQuota {
                arguments: GetQuotaArguments::parse(&arguments)
                    .expect("borrowed GETQUOTA arguments were already validated"),
            }
        }
        CommandBodyRef::GetQuotaRoot { arguments } => {
            let arguments = slice_for(frame, arguments);
            CommandBody::GetQuotaRoot {
                arguments: GetQuotaRootArguments::parse(&arguments)
                    .expect("borrowed GETQUOTAROOT arguments were already validated"),
            }
        }
        CommandBodyRef::SetQuota { arguments } => {
            let arguments = slice_for(frame, arguments);
            CommandBody::SetQuota {
                arguments: SetQuotaArguments::parse(&arguments)
                    .expect("borrowed SETQUOTA arguments were already validated"),
            }
        }
        CommandBodyRef::Status { mailbox, items } => CommandBody::Status {
            mailbox: slice_for(frame, mailbox),
            items: slice_for(frame, items),
        },
        CommandBodyRef::Append { mailbox, arguments } => CommandBody::Append {
            mailbox: slice_for(frame, mailbox),
            arguments: slice_for(frame, arguments),
        },
        CommandBodyRef::Id { parameters } => CommandBody::Id {
            parameters: slice_for(frame, parameters),
        },
        CommandBodyRef::Search { criteria } => CommandBody::Search {
            criteria: slice_for(frame, criteria),
        },
        CommandBodyRef::Sort { arguments } => {
            let arguments = slice_for(frame, arguments);
            CommandBody::Sort {
                arguments: SortArguments::parse_with_max_depth(&arguments, usize::MAX)
                    .expect("borrowed SORT arguments were already validated"),
            }
        }
        CommandBodyRef::Thread { arguments } => {
            let arguments = slice_for(frame, arguments);
            CommandBody::Thread {
                arguments: ThreadArguments::parse_with_max_depth(&arguments, usize::MAX)
                    .expect("borrowed THREAD arguments were already validated"),
            }
        }
        CommandBodyRef::Fetch {
            sequence_set,
            items,
        } => CommandBody::Fetch {
            sequence_set: sequence_set.into_owned(),
            items: slice_for(frame, items),
        },
        CommandBodyRef::Store {
            sequence_set,
            operation,
            silent,
            flags,
        } => CommandBody::Store {
            sequence_set: sequence_set.into_owned(),
            operation,
            silent,
            flags: slice_for(frame, flags),
        },
        CommandBodyRef::StoreConditional {
            sequence_set,
            unchanged_since,
            operation,
            silent,
            flags,
        } => CommandBody::StoreConditional {
            sequence_set: sequence_set.into_owned(),
            unchanged_since,
            operation,
            silent,
            flags: slice_for(frame, flags),
        },
        CommandBodyRef::Copy {
            sequence_set,
            mailbox,
        } => CommandBody::Copy {
            sequence_set: sequence_set.into_owned(),
            mailbox: slice_for(frame, mailbox),
        },
        CommandBodyRef::Move {
            sequence_set,
            mailbox,
        } => CommandBody::Move {
            sequence_set: sequence_set.into_owned(),
            mailbox: slice_for(frame, mailbox),
        },
        CommandBodyRef::Uid { command, arguments } => CommandBody::Uid {
            command: slice_for(frame, command),
            arguments: slice_for(frame, arguments),
        },
        CommandBodyRef::Raw { name, arguments } => CommandBody::Raw {
            name: slice_for(frame, name),
            arguments: slice_for(frame, arguments),
        },
    };
    Command {
        tag: slice_for(frame, command.tag),
        body,
    }
}

fn no_ref_args<'a>(
    arguments: &[u8],
    command: CommandBodyRef<'a>,
) -> Result<CommandBodyRef<'a>, ProtocolError> {
    if arguments.is_empty() {
        Ok(command)
    } else {
        Err(ProtocolError::new(
            ErrorKind::InvalidSyntax,
            "argument-free IMAP command",
        ))
    }
}

fn split_required_space<'a>(
    input: &'a [u8],
    context: &'static str,
) -> Result<(&'a [u8], &'a [u8]), ProtocolError> {
    let Some(boundary) = input.iter().position(|byte| *byte == b' ') else {
        return Err(ProtocolError::new(ErrorKind::InvalidSyntax, context));
    };
    let head = &input[..boundary];
    let tail = &input[boundary + 1..];
    if head.is_empty() || tail.is_empty() || tail.first() == Some(&b' ') {
        return Err(ProtocolError::new(ErrorKind::InvalidSyntax, context).at(boundary));
    }
    Ok((head, tail))
}

fn split_optional_arguments(input: &[u8]) -> Result<(&[u8], &[u8]), ProtocolError> {
    let Some(boundary) = input.iter().position(|byte| *byte == b' ') else {
        return Ok((input, &input[input.len()..]));
    };
    let name = &input[..boundary];
    let arguments = &input[boundary + 1..];
    if name.is_empty() || arguments.is_empty() || arguments.first() == Some(&b' ') {
        return Err(ProtocolError::new(
            ErrorKind::InvalidSyntax,
            "IMAP command argument separator",
        )
        .at(boundary));
    }
    Ok((name, arguments))
}

fn take_required_tail<'a>(
    input: &'a [u8],
    context: &'static str,
) -> Result<&'a [u8], ProtocolError> {
    if input.first() != Some(&b' ') || input.len() == 1 || input.get(1) == Some(&b' ') {
        return Err(ProtocolError::new(ErrorKind::InvalidSyntax, context));
    }
    Ok(&input[1..])
}

fn one_ref_argument<'a>(
    arguments: &'a [u8],
    context: &'static str,
) -> Result<&'a [u8], ProtocolError> {
    let (argument, remaining) = take_astring(arguments)?;
    if !remaining.is_empty() {
        return Err(ProtocolError::new(ErrorKind::InvalidSyntax, context));
    }
    Ok(argument)
}

fn two_ref_astrings<'a>(
    arguments: &'a [u8],
    context: &'static str,
) -> Result<(&'a [u8], &'a [u8]), ProtocolError> {
    let (first, remaining) = take_astring(arguments)?;
    let second_input = take_required_tail(remaining, context)?;
    let (second, tail) = take_astring(second_input)?;
    if !tail.is_empty() {
        return Err(ProtocolError::new(ErrorKind::InvalidSyntax, context));
    }
    Ok((first, second))
}

fn validate_atom_list(mut input: &[u8], context: &'static str) -> Result<(), ProtocolError> {
    let mut count = 0usize;
    while !input.is_empty() {
        let (atom, remaining) = split_token(input);
        validate_atom(atom, context)?;
        count += 1;
        input = remaining;
    }
    if count == 0 {
        Err(ProtocolError::new(ErrorKind::InvalidSyntax, context))
    } else {
        Ok(())
    }
}

fn own_atom_list(frame: &Bytes, mut input: &[u8]) -> Vec<Bytes> {
    let mut atoms = Vec::new();
    while !input.is_empty() {
        let (atom, remaining) = split_token(input);
        atoms.push(slice_for(frame, atom));
        input = remaining;
    }
    atoms
}

fn sequence_and_mailbox_ref<'a>(
    arguments: &'a [u8],
    context: &'static str,
) -> Result<(SequenceSetRef<'a>, &'a [u8]), ProtocolError> {
    let (sequence, remaining) = split_token(arguments);
    let sequence_set = SequenceSetRef::parse(sequence)?;
    let mailbox = one_ref_argument(remaining, context)?;
    Ok((sequence_set, mailbox))
}

fn parse_store_ref(arguments: &[u8]) -> Result<CommandBodyRef<'_>, ProtocolError> {
    let (sequence, remaining) =
        split_required_space(arguments, "IMAP STORE sequence-set separator")?;
    let sequence_set = SequenceSetRef::parse(sequence)?;
    let (unchanged_since, remaining) = parse_store_modifier(remaining)?;
    let (item, flags) = split_required_space(remaining, "IMAP STORE data item separator")?;
    let (operation, silent) = if eq_ascii(item, b"FLAGS") {
        (StoreOperation::Replace, false)
    } else if eq_ascii(item, b"FLAGS.SILENT") {
        (StoreOperation::Replace, true)
    } else if eq_ascii(item, b"+FLAGS") {
        (StoreOperation::Add, false)
    } else if eq_ascii(item, b"+FLAGS.SILENT") {
        (StoreOperation::Add, true)
    } else if eq_ascii(item, b"-FLAGS") {
        (StoreOperation::Remove, false)
    } else if eq_ascii(item, b"-FLAGS.SILENT") {
        (StoreOperation::Remove, true)
    } else {
        return Err(ProtocolError::new(
            ErrorKind::InvalidSyntax,
            "IMAP STORE data item",
        ));
    };
    validate_store_flags(flags)?;
    Ok(if let Some(unchanged_since) = unchanged_since {
        CommandBodyRef::StoreConditional {
            sequence_set,
            unchanged_since,
            operation,
            silent,
            flags,
        }
    } else {
        CommandBodyRef::Store {
            sequence_set,
            operation,
            silent,
            flags,
        }
    })
}

fn parse_store_modifier(input: &[u8]) -> Result<(Option<u64>, &[u8]), ProtocolError> {
    if input.first() != Some(&b'(') {
        return Ok((None, input));
    }
    let Some(close) = input.iter().position(|byte| *byte == b')') else {
        return Err(ProtocolError::new(
            ErrorKind::InvalidSyntax,
            "IMAP STORE modifier list",
        ));
    };
    let (name, value) =
        split_required_space(&input[1..close], "IMAP UNCHANGEDSINCE modifier separator")?;
    if !eq_ascii(name, b"UNCHANGEDSINCE") {
        return Err(ProtocolError::new(
            ErrorKind::InvalidSyntax,
            "unsupported IMAP STORE modifier",
        ));
    }
    let unchanged_since = parse_mod_sequence_valzer(value)?;
    if input.get(close + 1) != Some(&b' ')
        || input.get(close + 2).is_none()
        || input.get(close + 2) == Some(&b' ')
    {
        return Err(ProtocolError::new(
            ErrorKind::InvalidSyntax,
            "IMAP STORE modifier list separator",
        ));
    }
    Ok((Some(unchanged_since), &input[close + 2..]))
}

fn parse_mod_sequence_valzer(input: &[u8]) -> Result<u64, ProtocolError> {
    if input.is_empty() || !input.iter().all(u8::is_ascii_digit) {
        return Err(ProtocolError::new(
            ErrorKind::InvalidSyntax,
            "IMAP UNCHANGEDSINCE mod-sequence",
        ));
    }
    let value = input.iter().try_fold(0u64, |value, digit| {
        value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(*digit - b'0')))
    });
    match value {
        Some(value) if i64::try_from(value).is_ok() => Ok(value),
        _ => Err(ProtocolError::new(
            ErrorKind::InvalidSyntax,
            "IMAP UNCHANGEDSINCE mod-sequence",
        )),
    }
}

pub(super) fn validate_uid_arguments(
    command: &[u8],
    arguments: &[u8],
    max_depth: usize,
) -> Result<(), ProtocolError> {
    if eq_ascii(command, b"SEARCH") {
        validate_search_program(arguments, max_depth)
    } else if eq_ascii(command, b"SORT") {
        validate_sort_arguments(arguments, max_depth).map(|_| ())
    } else if eq_ascii(command, b"THREAD") {
        validate_thread_arguments(arguments, max_depth).map(|_| ())
    } else if eq_ascii(command, b"FETCH") {
        validate_uid_fetch_arguments(arguments, max_depth)
    } else if eq_ascii(command, b"STORE") {
        parse_store_ref(arguments).map(|_| ())
    } else if eq_ascii(command, b"COPY") {
        sequence_and_mailbox_ref(arguments, "IMAP UID COPY").map(|_| ())
    } else if eq_ascii(command, b"MOVE") {
        sequence_and_mailbox_ref(arguments, "IMAP UID MOVE").map(|_| ())
    } else if eq_ascii(command, b"EXPUNGE") {
        SequenceSetRef::parse(arguments).map(|_| ())
    } else {
        Ok(())
    }
}

fn required_raw(value: &[u8], context: &'static str) -> Result<(), ProtocolError> {
    if value.is_empty() {
        Err(ProtocolError::new(ErrorKind::InvalidSyntax, context))
    } else {
        validate_raw(value)
    }
}

pub(super) fn encode_astring_command(
    name: &[u8],
    value: &[u8],
    context: &'static str,
    dst: &mut BytesMut,
) -> Result<(), ProtocolError> {
    validate_astring_argument(value, context)?;
    dst.put_slice(name);
    dst.put_u8(b' ');
    dst.put_slice(value);
    Ok(())
}

pub(super) fn validate_astring_argument(
    value: &[u8],
    context: &'static str,
) -> Result<(), ProtocolError> {
    validate_astring(value).map_err(|error| ProtocolError::new(error.kind(), context))
}

pub(super) fn encode_raw_command(
    name: &[u8],
    arguments: &[u8],
    context: &'static str,
    dst: &mut BytesMut,
) -> Result<(), ProtocolError> {
    required_raw(arguments, context)?;
    dst.put_slice(name);
    dst.put_u8(b' ');
    dst.put_slice(arguments);
    Ok(())
}

fn take_astring(input: &[u8]) -> Result<(&[u8], &[u8]), ProtocolError> {
    let parsed = parse_astring_prefix(input)?;
    Ok((&input[..parsed.end], &input[parsed.end..]))
}
