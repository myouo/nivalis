use mail_protocol_core::wire::eq_ascii;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CommandKind {
    Capability,
    Noop,
    Logout,
    StartTls,
    Idle,
    Check,
    Close,
    Expunge,
    Unselect,
    Namespace,
    GetQuota,
    GetQuotaRoot,
    SetQuota,
    Select,
    Examine,
    Login,
    Authenticate,
    Enable,
    Create,
    Delete,
    Rename,
    Subscribe,
    Unsubscribe,
    List,
    Lsub,
    Status,
    Append,
    Id,
    Search,
    Sort,
    Thread,
    Fetch,
    Store,
    Copy,
    Move,
    Uid,
    Raw,
}

#[inline]
#[allow(clippy::too_many_lines)]
pub(crate) fn classify_command(name: &[u8]) -> CommandKind {
    match name.len() {
        2 if eq_ascii(name, b"ID") => CommandKind::Id,
        3 if eq_ascii(name, b"UID") => CommandKind::Uid,
        4 if eq_ascii(name, b"NOOP") => CommandKind::Noop,
        4 if eq_ascii(name, b"IDLE") => CommandKind::Idle,
        4 if eq_ascii(name, b"LIST") => CommandKind::List,
        4 if eq_ascii(name, b"LSUB") => CommandKind::Lsub,
        4 if eq_ascii(name, b"COPY") => CommandKind::Copy,
        4 if eq_ascii(name, b"MOVE") => CommandKind::Move,
        4 if eq_ascii(name, b"SORT") => CommandKind::Sort,
        5 if eq_ascii(name, b"FETCH") => CommandKind::Fetch,
        5 if eq_ascii(name, b"LOGIN") => CommandKind::Login,
        5 if eq_ascii(name, b"CHECK") => CommandKind::Check,
        5 if eq_ascii(name, b"CLOSE") => CommandKind::Close,
        5 if eq_ascii(name, b"STORE") => CommandKind::Store,
        6 if eq_ascii(name, b"SELECT") => CommandKind::Select,
        6 if eq_ascii(name, b"STATUS") => CommandKind::Status,
        6 if eq_ascii(name, b"SEARCH") => CommandKind::Search,
        6 if eq_ascii(name, b"THREAD") => CommandKind::Thread,
        6 if eq_ascii(name, b"APPEND") => CommandKind::Append,
        6 if eq_ascii(name, b"LOGOUT") => CommandKind::Logout,
        6 if eq_ascii(name, b"ENABLE") => CommandKind::Enable,
        6 if eq_ascii(name, b"CREATE") => CommandKind::Create,
        6 if eq_ascii(name, b"DELETE") => CommandKind::Delete,
        6 if eq_ascii(name, b"RENAME") => CommandKind::Rename,
        7 if eq_ascii(name, b"EXAMINE") => CommandKind::Examine,
        7 if eq_ascii(name, b"EXPUNGE") => CommandKind::Expunge,
        8 if eq_ascii(name, b"STARTTLS") => CommandKind::StartTls,
        8 if eq_ascii(name, b"UNSELECT") => CommandKind::Unselect,
        8 if eq_ascii(name, b"GETQUOTA") => CommandKind::GetQuota,
        8 if eq_ascii(name, b"SETQUOTA") => CommandKind::SetQuota,
        9 if eq_ascii(name, b"NAMESPACE") => CommandKind::Namespace,
        9 if eq_ascii(name, b"SUBSCRIBE") => CommandKind::Subscribe,
        10 if eq_ascii(name, b"CAPABILITY") => CommandKind::Capability,
        11 if eq_ascii(name, b"UNSUBSCRIBE") => CommandKind::Unsubscribe,
        12 if eq_ascii(name, b"AUTHENTICATE") => CommandKind::Authenticate,
        12 if eq_ascii(name, b"GETQUOTAROOT") => CommandKind::GetQuotaRoot,
        _ => CommandKind::Raw,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifier_is_case_insensitive_and_preserves_extensions() {
        for (name, expected) in [
            (b"CAPABILITY".as_slice(), CommandKind::Capability),
            (b"NOOP".as_slice(), CommandKind::Noop),
            (b"LOGOUT".as_slice(), CommandKind::Logout),
            (b"STARTTLS".as_slice(), CommandKind::StartTls),
            (b"IDLE".as_slice(), CommandKind::Idle),
            (b"CHECK".as_slice(), CommandKind::Check),
            (b"CLOSE".as_slice(), CommandKind::Close),
            (b"EXPUNGE".as_slice(), CommandKind::Expunge),
            (b"UNSELECT".as_slice(), CommandKind::Unselect),
            (b"NAMESPACE".as_slice(), CommandKind::Namespace),
            (b"GETQUOTA".as_slice(), CommandKind::GetQuota),
            (b"GETQUOTAROOT".as_slice(), CommandKind::GetQuotaRoot),
            (b"SETQUOTA".as_slice(), CommandKind::SetQuota),
            (b"SELECT".as_slice(), CommandKind::Select),
            (b"EXAMINE".as_slice(), CommandKind::Examine),
            (b"LOGIN".as_slice(), CommandKind::Login),
            (b"AUTHENTICATE".as_slice(), CommandKind::Authenticate),
            (b"ENABLE".as_slice(), CommandKind::Enable),
            (b"CREATE".as_slice(), CommandKind::Create),
            (b"DELETE".as_slice(), CommandKind::Delete),
            (b"RENAME".as_slice(), CommandKind::Rename),
            (b"SUBSCRIBE".as_slice(), CommandKind::Subscribe),
            (b"UNSUBSCRIBE".as_slice(), CommandKind::Unsubscribe),
            (b"LIST".as_slice(), CommandKind::List),
            (b"LSUB".as_slice(), CommandKind::Lsub),
            (b"STATUS".as_slice(), CommandKind::Status),
            (b"APPEND".as_slice(), CommandKind::Append),
            (b"ID".as_slice(), CommandKind::Id),
            (b"SEARCH".as_slice(), CommandKind::Search),
            (b"SORT".as_slice(), CommandKind::Sort),
            (b"THREAD".as_slice(), CommandKind::Thread),
            (b"FETCH".as_slice(), CommandKind::Fetch),
            (b"STORE".as_slice(), CommandKind::Store),
            (b"COPY".as_slice(), CommandKind::Copy),
            (b"MOVE".as_slice(), CommandKind::Move),
            (b"UID".as_slice(), CommandKind::Uid),
        ] {
            assert_eq!(classify_command(name), expected);
            assert_eq!(classify_command(&name.to_ascii_lowercase()), expected);
        }
        assert_eq!(classify_command(b"X-VENDOR"), CommandKind::Raw);
    }
}
