use super::{
    BufMut, BytesMut, Command, CommandBody, DEFAULT_FETCH_MAX_DEPTH, DEFAULT_LIST_MAX_DEPTH,
    DEFAULT_MAX_DEPTH, Encoder, ErrorKind, ProtocolError, StoreOperation, append_transactionally,
    encode_astring_command, encode_raw_command, validate_append_arguments,
    validate_astring_argument, validate_atom, validate_fetch_arguments, validate_id_command,
    validate_initial_response, validate_list_arguments, validate_raw, validate_search_program,
    validate_status_items, validate_store_flags, validate_tag, validate_uid_arguments,
};

/// Encoder for complete IMAP command frames.
///
/// This encoder writes synchronizing literal payloads into the destination as
/// part of the complete frame. A client that sends the resulting bytes must not
/// write them all at once; prefer [`crate::ClientCommandTransmission`] to release each
/// payload only after its server continuation.
#[derive(Clone, Copy, Debug, Default)]
pub struct CommandEncoder;

impl Encoder<Command> for CommandEncoder {
    #[allow(clippy::too_many_lines)]
    fn encode(&mut self, item: &Command, dst: &mut BytesMut) -> Result<(), ProtocolError> {
        append_transactionally(dst, |dst| encode_command(item, dst))
    }
}

#[allow(clippy::too_many_lines)]
fn encode_command(item: &Command, dst: &mut BytesMut) -> Result<(), ProtocolError> {
    validate_tag(&item.tag)?;
    dst.reserve(item.tag.len() + 32);
    dst.put_slice(&item.tag);
    dst.put_u8(b' ');
    match &item.body {
        CommandBody::Capability => dst.put_slice(b"CAPABILITY"),
        CommandBody::Noop => dst.put_slice(b"NOOP"),
        CommandBody::Logout => dst.put_slice(b"LOGOUT"),
        CommandBody::StartTls => dst.put_slice(b"STARTTLS"),
        CommandBody::Idle => dst.put_slice(b"IDLE"),
        CommandBody::Check => dst.put_slice(b"CHECK"),
        CommandBody::Close => dst.put_slice(b"CLOSE"),
        CommandBody::Expunge => dst.put_slice(b"EXPUNGE"),
        CommandBody::Login { username, password } => {
            validate_astring_argument(username, "IMAP LOGIN username")?;
            validate_astring_argument(password, "IMAP LOGIN password")?;
            dst.put_slice(b"LOGIN ");
            dst.put_slice(username);
            dst.put_u8(b' ');
            dst.put_slice(password);
        }
        CommandBody::Authenticate {
            mechanism,
            initial_response,
        } => {
            validate_atom(mechanism, "IMAP AUTHENTICATE mechanism")?;
            dst.put_slice(b"AUTHENTICATE ");
            dst.put_slice(mechanism);
            if let Some(initial_response) = initial_response {
                validate_initial_response(initial_response)?;
                dst.put_u8(b' ');
                dst.put_slice(initial_response);
            }
        }
        CommandBody::Enable { capabilities } => {
            if capabilities.is_empty() {
                return Err(ProtocolError::new(
                    ErrorKind::InvalidSyntax,
                    "IMAP ENABLE capabilities",
                ));
            }
            dst.put_slice(b"ENABLE");
            for capability in capabilities {
                validate_atom(capability, "IMAP ENABLE capability")?;
                dst.put_u8(b' ');
                dst.put_slice(capability);
            }
        }
        CommandBody::Select { mailbox } => {
            validate_astring_argument(mailbox, "IMAP SELECT mailbox")?;
            dst.put_slice(b"SELECT ");
            dst.put_slice(mailbox);
        }
        CommandBody::SelectExtended { arguments } => {
            dst.put_slice(b"SELECT ");
            arguments.encode(dst);
        }
        CommandBody::Examine { mailbox } => {
            validate_astring_argument(mailbox, "IMAP EXAMINE mailbox")?;
            dst.put_slice(b"EXAMINE ");
            dst.put_slice(mailbox);
        }
        CommandBody::ExamineExtended { arguments } => {
            dst.put_slice(b"EXAMINE ");
            arguments.encode(dst);
        }
        CommandBody::Unselect => dst.put_slice(b"UNSELECT"),
        CommandBody::Create { mailbox } => {
            encode_astring_command(b"CREATE", mailbox, "IMAP CREATE mailbox", dst)?;
        }
        CommandBody::Delete { mailbox } => {
            encode_astring_command(b"DELETE", mailbox, "IMAP DELETE mailbox", dst)?;
        }
        CommandBody::Rename { from, to } => {
            validate_astring_argument(from, "IMAP RENAME source mailbox")?;
            validate_astring_argument(to, "IMAP RENAME destination mailbox")?;
            dst.put_slice(b"RENAME ");
            dst.put_slice(from);
            dst.put_u8(b' ');
            dst.put_slice(to);
        }
        CommandBody::Subscribe { mailbox } => {
            encode_astring_command(b"SUBSCRIBE", mailbox, "IMAP SUBSCRIBE mailbox", dst)?;
        }
        CommandBody::Unsubscribe { mailbox } => {
            encode_astring_command(b"UNSUBSCRIBE", mailbox, "IMAP UNSUBSCRIBE mailbox", dst)?;
        }
        CommandBody::List { arguments } => {
            validate_list_arguments(arguments, DEFAULT_LIST_MAX_DEPTH)?;
            dst.put_slice(b"LIST ");
            dst.put_slice(arguments);
        }
        CommandBody::Lsub { arguments } => {
            encode_raw_command(b"LSUB", arguments, "IMAP LSUB arguments", dst)?;
        }
        CommandBody::Namespace => dst.put_slice(b"NAMESPACE"),
        CommandBody::GetQuota { arguments } => {
            dst.put_slice(b"GETQUOTA ");
            arguments.encode(dst);
        }
        CommandBody::GetQuotaRoot { arguments } => {
            dst.put_slice(b"GETQUOTAROOT ");
            arguments.encode(dst);
        }
        CommandBody::SetQuota { arguments } => {
            dst.put_slice(b"SETQUOTA ");
            arguments.encode(dst);
        }
        CommandBody::Status { mailbox, items } => {
            validate_astring_argument(mailbox, "IMAP STATUS mailbox")?;
            validate_status_items(items)?;
            dst.put_slice(b"STATUS ");
            dst.put_slice(mailbox);
            dst.put_u8(b' ');
            dst.put_slice(items);
        }
        CommandBody::Append { mailbox, arguments } => {
            validate_astring_argument(mailbox, "IMAP APPEND mailbox")?;
            validate_append_arguments(arguments)?;
            dst.put_slice(b"APPEND ");
            dst.put_slice(mailbox);
            dst.put_u8(b' ');
            dst.put_slice(arguments);
        }
        CommandBody::Id { parameters } => {
            validate_id_command(parameters)?;
            dst.put_slice(b"ID ");
            dst.put_slice(parameters);
        }
        CommandBody::Search { criteria } => {
            validate_search_program(criteria, DEFAULT_MAX_DEPTH)?;
            dst.put_slice(b"SEARCH ");
            dst.put_slice(criteria);
        }
        CommandBody::Sort { arguments } => {
            dst.put_slice(b"SORT ");
            arguments.encode(dst);
        }
        CommandBody::Thread { arguments } => {
            dst.put_slice(b"THREAD ");
            arguments.encode(dst);
        }
        CommandBody::Fetch {
            sequence_set,
            items,
        } => {
            validate_fetch_arguments(items, DEFAULT_FETCH_MAX_DEPTH, false)?;
            dst.put_slice(b"FETCH ");
            sequence_set.encode(dst);
            dst.put_u8(b' ');
            dst.put_slice(items);
        }
        CommandBody::Store {
            sequence_set,
            operation,
            silent,
            flags,
        } => {
            validate_store_flags(flags)?;
            dst.put_slice(b"STORE ");
            sequence_set.encode(dst);
            dst.put_u8(b' ');
            dst.put_slice(match operation {
                StoreOperation::Replace => b"FLAGS".as_slice(),
                StoreOperation::Add => b"+FLAGS".as_slice(),
                StoreOperation::Remove => b"-FLAGS".as_slice(),
            });
            if *silent {
                dst.put_slice(b".SILENT");
            }
            dst.put_u8(b' ');
            dst.put_slice(flags);
        }
        CommandBody::StoreConditional {
            sequence_set,
            unchanged_since,
            operation,
            silent,
            flags,
        } => {
            validate_store_flags(flags)?;
            if *unchanged_since > i64::MAX as u64 {
                return Err(ProtocolError::new(
                    ErrorKind::InvalidSyntax,
                    "IMAP UNCHANGEDSINCE mod-sequence",
                ));
            }
            dst.put_slice(b"STORE ");
            sequence_set.encode(dst);
            dst.put_slice(b" (UNCHANGEDSINCE ");
            put_u64(*unchanged_since, dst);
            dst.put_slice(b") ");
            dst.put_slice(match operation {
                StoreOperation::Replace => b"FLAGS".as_slice(),
                StoreOperation::Add => b"+FLAGS".as_slice(),
                StoreOperation::Remove => b"-FLAGS".as_slice(),
            });
            if *silent {
                dst.put_slice(b".SILENT");
            }
            dst.put_u8(b' ');
            dst.put_slice(flags);
        }
        CommandBody::Copy {
            sequence_set,
            mailbox,
        }
        | CommandBody::Move {
            sequence_set,
            mailbox,
        } => {
            validate_astring_argument(mailbox, "IMAP COPY/MOVE mailbox")?;
            dst.put_slice(if matches!(item.body, CommandBody::Copy { .. }) {
                b"COPY "
            } else {
                b"MOVE "
            });
            sequence_set.encode(dst);
            dst.put_u8(b' ');
            dst.put_slice(mailbox);
        }
        CommandBody::Uid { command, arguments } => {
            validate_atom(command, "IMAP UID subcommand")?;
            validate_raw(arguments)?;
            if arguments.is_empty() {
                return Err(ProtocolError::new(
                    ErrorKind::InvalidSyntax,
                    "IMAP UID arguments",
                ));
            }
            validate_uid_arguments(command, arguments, DEFAULT_FETCH_MAX_DEPTH)?;
            dst.put_slice(b"UID ");
            dst.put_slice(command);
            dst.put_u8(b' ');
            dst.put_slice(arguments);
        }
        CommandBody::Raw { name, arguments } => {
            validate_atom(name, "IMAP command name")?;
            validate_raw(arguments)?;
            dst.put_slice(name);
            if !arguments.is_empty() {
                dst.put_u8(b' ');
                dst.put_slice(arguments);
            }
        }
    }
    dst.put_slice(b"\r\n");
    Ok(())
}

fn put_u64(mut value: u64, dst: &mut BytesMut) {
    let mut digits = [0u8; 20];
    let mut cursor = digits.len();
    loop {
        cursor -= 1;
        digits[cursor] = b'0' + u8::try_from(value % 10).expect("one decimal digit");
        value /= 10;
        if value == 0 {
            break;
        }
    }
    dst.put_slice(&digits[cursor..]);
}
