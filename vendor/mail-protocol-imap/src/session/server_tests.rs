use super::*;

fn command(tag: &'static [u8], body: crate::CommandBody) -> Command {
    Command {
        tag: Bytes::from_static(tag),
        body,
    }
}

fn untagged(data: &'static [u8]) -> Response {
    Response::Untagged {
        data: Bytes::from_static(data),
    }
}

fn tagged(tag: &'static [u8], status: Status) -> Response {
    Response::Tagged {
        tag: Bytes::from_static(tag),
        status,
        information: Bytes::new(),
    }
}

fn configured(capabilities: impl IntoIterator<Item = Capability>) -> ServerSession {
    let mut set = CapabilitySet::default();
    for capability in capabilities {
        set.insert(capability);
    }
    ServerSession::new(set)
}

#[test]
fn login_select_and_close_follow_confirmed_responses() {
    let mut session = ServerSession::default();
    session.on_greeting_sent(&untagged(b"OK ready")).unwrap();
    session
        .on_command(&command(
            b"A1",
            crate::CommandBody::Login {
                username: Bytes::from_static(b"user"),
                password: Bytes::from_static(b"pass"),
            },
        ))
        .unwrap();
    session
        .on_response_sent(&tagged(b"A1", Status::Ok))
        .unwrap();
    session
        .on_command(&command(
            b"A2",
            crate::CommandBody::Select {
                mailbox: Bytes::from_static(b"INBOX"),
            },
        ))
        .unwrap();
    session
        .on_response_sent(&tagged(b"A2", Status::Ok))
        .unwrap();
    assert_eq!(session.state(), SessionState::Selected { read_only: false });

    session
        .on_command(&command(b"A3", crate::CommandBody::Close))
        .unwrap();
    session
        .on_response_sent(&tagged(b"A3", Status::Ok))
        .unwrap();
    assert_eq!(session.state(), SessionState::Authenticated);
}

#[test]
fn server_requires_enabled_qresync_for_select_and_vanished() {
    let mut session = configured([
        Capability::Enable,
        Capability::CondStore,
        Capability::QResync,
    ]);
    session
        .on_greeting_sent(&untagged(b"PREAUTH ready"))
        .unwrap();
    let select = Command::parse(Bytes::from_static(
        b"S1 SELECT INBOX (QRESYNC (777 12345))\r\n",
    ))
    .unwrap();
    assert!(session.on_command(&select).is_err());
    assert!(
        session
            .on_response_sent(&untagged(b"VANISHED 1:3"))
            .is_err()
    );

    session
        .on_command(&command(
            b"E1",
            crate::CommandBody::Enable {
                capabilities: vec![Bytes::from_static(b"QRESYNC")],
            },
        ))
        .unwrap();
    session
        .on_response_sent(&untagged(b"ENABLED QRESYNC"))
        .unwrap();
    session
        .on_response_sent(&tagged(b"E1", Status::Ok))
        .unwrap();
    assert!(
        session
            .enabled_capabilities()
            .contains(&Capability::QResync)
    );

    session.on_command(&select).unwrap();
    assert!(
        session
            .on_response_sent(&untagged(b"VANISHED (EARLIER) 1:3"))
            .is_ok()
    );
}

#[test]
fn server_enforces_sort_thread_and_quota_capability_boundaries() {
    let sort = Command::parse(Bytes::from_static(b"S1 UID SORT (DATE) UTF-8 ALL\r\n")).unwrap();
    let thread = Command::parse(Bytes::from_static(
        b"T1 UID THREAD REFERENCES UTF-8 ALL\r\n",
    ))
    .unwrap();
    let set = Command::parse(Bytes::from_static(
        b"Q1 SETQUOTA \"\" (STORAGE 100 MESSAGE 20)\r\n",
    ))
    .unwrap();
    let mut session = configured([
        Capability::Sort,
        Capability::Thread {
            algorithm: Bytes::from_static(b"references"),
        },
        Capability::Quota,
        Capability::QuotaSet,
        Capability::QuotaResource {
            resource: Bytes::from_static(b"STORAGE"),
        },
        Capability::QuotaResource {
            resource: Bytes::from_static(b"MESSAGE"),
        },
    ]);
    session
        .on_greeting_sent(&untagged(b"PREAUTH ready"))
        .unwrap();
    assert!(session.on_command(&sort).is_err());
    session
        .on_command(&command(
            b"SELECT",
            crate::CommandBody::Select {
                mailbox: Bytes::from_static(b"INBOX"),
            },
        ))
        .unwrap();
    session
        .on_response_sent(&tagged(b"SELECT", Status::Ok))
        .unwrap();
    session.on_command(&sort).unwrap();
    session
        .on_response_sent(&tagged(b"S1", Status::Ok))
        .unwrap();
    session.on_command(&thread).unwrap();
    session
        .on_response_sent(&tagged(b"T1", Status::Ok))
        .unwrap();
    session.on_command(&set).unwrap();
}

#[test]
fn authenticate_supports_multiple_challenges_and_strict_cancel() {
    let mut session = ServerSession::default();
    session.on_greeting_sent(&untagged(b"OK ready")).unwrap();
    session
        .on_command(&command(
            b"A1",
            crate::CommandBody::Authenticate {
                mechanism: Bytes::from_static(b"PLAIN"),
                initial_response: None,
            },
        ))
        .unwrap();
    assert!(
        session
            .on_response_sent(&Response::Continuation {
                data: Bytes::from_static(b"not-base64"),
            })
            .is_err()
    );
    session
        .on_response_sent(&Response::Continuation { data: Bytes::new() })
        .unwrap();
    assert!(
        session
            .on_authenticate_continuation(&AuthenticateContinuation::Response(Bytes::from_static(
                b"not-base64"
            ),))
            .is_err()
    );
    session
        .on_authenticate_continuation(&AuthenticateContinuation::Response(Bytes::from_static(
            b"dGVzdA==",
        )))
        .unwrap();
    session
        .on_response_sent(&Response::Continuation {
            data: Bytes::from_static(b"bmV4dA=="),
        })
        .unwrap();
    session
        .on_authenticate_continuation(&AuthenticateContinuation::Cancel)
        .unwrap();
    assert!(
        session
            .on_response_sent(&tagged(b"A1", Status::No))
            .is_err()
    );
    session
        .on_response_sent(&tagged(b"A1", Status::Bad))
        .unwrap();
    assert_eq!(session.state(), SessionState::NotAuthenticated);
}

#[test]
fn idle_requires_continuation_then_done_then_ok() {
    let mut session = configured([Capability::Imap4Rev2]);
    session
        .on_greeting_sent(&untagged(b"PREAUTH ready"))
        .unwrap();
    session
        .on_command(&command(b"A1", crate::CommandBody::Idle))
        .unwrap();
    assert!(session.on_idle_done(IdleDone).is_err());
    session.on_response_sent(&untagged(b"2 EXPUNGE")).unwrap();
    assert!(
        session
            .on_command(&command(b"A2", crate::CommandBody::Noop))
            .is_err()
    );
    session
        .on_response_sent(&Response::Continuation {
            data: Bytes::from_static(b"idling"),
        })
        .unwrap();
    assert!(
        session
            .on_response_sent(&tagged(b"A1", Status::Ok))
            .is_err()
    );
    assert!(
        session
            .on_response_sent(&tagged(b"A1", Status::Bad))
            .is_err()
    );
    session.on_idle_done(IdleDone).unwrap();
    session.on_response_sent(&untagged(b"4 EXISTS")).unwrap();
    session
        .on_response_sent(&tagged(b"A1", Status::Ok))
        .unwrap();
}

#[test]
fn idle_may_fail_after_done_or_end_with_inactivity_bye() {
    for status in [Status::No, Status::Bad] {
        let mut session = configured([Capability::Idle]);
        session
            .on_greeting_sent(&untagged(b"PREAUTH ready"))
            .unwrap();
        session
            .on_command(&command(b"A1", crate::CommandBody::Idle))
            .unwrap();
        session
            .on_response_sent(&Response::Continuation {
                data: Bytes::from_static(b"idling"),
            })
            .unwrap();
        session.on_idle_done(IdleDone).unwrap();
        session.on_response_sent(&tagged(b"A1", status)).unwrap();
        assert_eq!(session.pending_command(), None);
    }

    let mut timed_out = configured([Capability::Idle]);
    timed_out
        .on_greeting_sent(&untagged(b"PREAUTH ready"))
        .unwrap();
    timed_out
        .on_command(&command(b"A1", crate::CommandBody::Idle))
        .unwrap();
    timed_out
        .on_response_sent(&Response::Continuation {
            data: Bytes::from_static(b"idling"),
        })
        .unwrap();
    assert_eq!(
        timed_out
            .on_response_sent(&untagged(b"BYE inactivity timeout"))
            .unwrap(),
        ServerSessionEvent::ServerBye
    );
    assert_eq!(timed_out.state(), SessionState::Logout);
}

#[test]
fn enable_requires_one_matching_enabled_response_before_success() {
    let mut session = configured([
        Capability::Enable,
        Capability::CondStore,
        Capability::Utf8Accept,
    ]);
    session
        .on_greeting_sent(&untagged(b"PREAUTH ready"))
        .unwrap();
    assert!(
        session
            .on_response_sent(&untagged(b"ENABLED CONDSTORE"))
            .is_err()
    );

    session
        .on_command(&command(
            b"A1",
            crate::CommandBody::Enable {
                capabilities: vec![Bytes::from_static(b"CONDSTORE")],
            },
        ))
        .unwrap();
    assert!(
        session
            .on_response_sent(&tagged(b"A1", Status::Ok))
            .is_err()
    );
    assert!(
        session
            .on_response_sent(&untagged(b"ENABLED UTF8=ACCEPT"))
            .is_err()
    );
    assert!(matches!(
        session
            .on_response_sent(&untagged(b"ENABLED CONDSTORE"))
            .unwrap(),
        ServerSessionEvent::CapabilitiesEnabled { capabilities }
            if capabilities.contains(&Capability::CondStore)
    ));
    assert!(session.enabled_capabilities().is_empty());
    assert!(session.on_response_sent(&untagged(b"ENABLED")).is_err());
    assert!(
        session
            .on_response_sent(&tagged(b"A1", Status::Bad))
            .is_err()
    );
    session
        .on_response_sent(&tagged(b"A1", Status::Ok))
        .unwrap();
    assert!(
        session
            .enabled_capabilities()
            .contains(&Capability::CondStore)
    );

    session
        .on_command(&command(
            b"A2",
            crate::CommandBody::Enable {
                capabilities: vec![Bytes::from_static(b"UTF8=ACCEPT")],
            },
        ))
        .unwrap();
    session
        .on_response_sent(&untagged(b"ENABLED UTF8=ACCEPT"))
        .unwrap();
    session
        .on_response_sent(&tagged(b"A2", Status::Ok))
        .unwrap();
    assert_eq!(session.enabled_capabilities().len(), 2);
}

#[test]
fn enable_allows_failure_without_enabled_and_empty_success() {
    let mut session = configured([
        Capability::Enable,
        Capability::Other {
            token: Bytes::from_static(b"X-OPTION"),
        },
    ]);
    session
        .on_greeting_sent(&untagged(b"PREAUTH ready"))
        .unwrap();

    for status in [Status::No, Status::Bad] {
        session
            .on_command(&command(
                b"FAIL",
                crate::CommandBody::Enable {
                    capabilities: vec![Bytes::from_static(b"X-OPTION")],
                },
            ))
            .unwrap();
        session.on_response_sent(&tagged(b"FAIL", status)).unwrap();
    }

    session
        .on_command(&command(
            b"EMPTY",
            crate::CommandBody::Enable {
                capabilities: vec![Bytes::from_static(b"X-OPTION")],
            },
        ))
        .unwrap();
    session.on_response_sent(&untagged(b"ENABLED")).unwrap();
    session
        .on_response_sent(&tagged(b"EMPTY", Status::Ok))
        .unwrap();
    assert!(session.enabled_capabilities().is_empty());
}

#[test]
fn starttls_waits_for_external_handshake() {
    let mut session = configured([Capability::Imap4Rev2, Capability::StartTls]);
    session.on_greeting_sent(&untagged(b"OK ready")).unwrap();
    session
        .on_command(&command(b"A1", crate::CommandBody::StartTls))
        .unwrap();
    session
        .on_response_sent(&tagged(b"A1", Status::Ok))
        .unwrap();
    assert_eq!(session.security(), SecurityState::TlsHandshake);
    assert!(
        session
            .on_command(&command(b"A2", crate::CommandBody::Capability))
            .is_err()
    );
    assert!(
        session
            .on_response_sent(&untagged(b"OK injected plaintext"))
            .is_err()
    );
    session.on_tls_established().unwrap();
    assert_eq!(session.security(), SecurityState::Tls);
    assert!(
        !session
            .advertised_capabilities()
            .contains(&Capability::StartTls)
    );
}

#[test]
fn esearch_must_match_server_search_command_and_uid_kind() {
    let mut session = ServerSession::default();
    session
        .on_greeting_sent(&untagged(b"PREAUTH ready"))
        .unwrap();
    session
        .on_command(&command(
            b"S",
            crate::CommandBody::Select {
                mailbox: Bytes::from_static(b"INBOX"),
            },
        ))
        .unwrap();
    session.on_response_sent(&tagged(b"S", Status::Ok)).unwrap();

    session
        .on_command(&command(
            b"A1",
            crate::CommandBody::Search {
                criteria: Bytes::from_static(b"RETURN (COUNT) UNSEEN"),
            },
        ))
        .unwrap();
    assert!(
        session
            .on_response_sent(&untagged(b"ESEARCH (TAG A2) COUNT 1"))
            .is_err()
    );
    assert!(
        session
            .on_response_sent(&untagged(b"ESEARCH (TAG A1) UID COUNT 1"))
            .is_err()
    );
    session
        .on_response_sent(&untagged(b"ESEARCH (TAG A1) COUNT 1"))
        .unwrap();
    session
        .on_response_sent(&tagged(b"A1", Status::Ok))
        .unwrap();

    session
        .on_command(&command(
            b"U1",
            crate::CommandBody::Uid {
                command: Bytes::from_static(b"SEARCH"),
                arguments: Bytes::from_static(b"RETURN (ALL) UNSEEN"),
            },
        ))
        .unwrap();
    assert!(
        session
            .on_response_sent(&untagged(b"ESEARCH (TAG U1) ALL 3"))
            .is_err()
    );
    session
        .on_response_sent(&untagged(b"ESEARCH (TAG U1) UID ALL 3"))
        .unwrap();
    session
        .on_response_sent(&tagged(b"U1", Status::Ok))
        .unwrap();

    session
        .on_response_sent(&untagged(b"ESEARCH COUNT 0"))
        .unwrap();
}

#[test]
fn childinfo_requires_matching_recursive_list_on_server() {
    let mut session = ServerSession::default();
    session
        .on_greeting_sent(&untagged(b"PREAUTH ready"))
        .unwrap();
    let child_info = untagged(b"LIST () \"/\" parent (CHILDINFO (\"SUBSCRIBED\"))");

    assert!(session.on_response_sent(&child_info).is_err());
    assert_eq!(
        session
            .on_response_sent(&untagged(b"LIST () \"/\" unsolicited"))
            .unwrap(),
        ServerSessionEvent::Untagged
    );

    session
        .on_command(&command(
            b"BASIC",
            crate::CommandBody::List {
                arguments: Bytes::from_static(b"\"\" *"),
            },
        ))
        .unwrap();
    assert!(session.on_response_sent(&child_info).is_err());
    assert_eq!(session.pending_command(), Some(PendingCommand::List));
    session
        .on_response_sent(&tagged(b"BASIC", Status::Ok))
        .unwrap();

    session
        .on_command(&command(
            b"RECURSIVE",
            crate::CommandBody::List {
                arguments: Bytes::from_static(b"(SUBSCRIBED RECURSIVEMATCH) \"\" *"),
            },
        ))
        .unwrap();
    assert!(
        session
            .on_response_sent(&untagged(b"LIST () \"/\" parent (CHILDINFO (\"X-FLAG\"))"))
            .is_err()
    );
    assert_eq!(
        session.pending_tag().map(Bytes::as_ref),
        Some(b"RECURSIVE".as_slice())
    );
    assert_eq!(
        session.on_response_sent(&child_info).unwrap(),
        ServerSessionEvent::Untagged
    );
    session
        .on_response_sent(&tagged(b"RECURSIVE", Status::Ok))
        .unwrap();
}

#[test]
fn saved_search_updates_are_explicit_and_uidvalidity_resets() {
    let mut session = ServerSession::default();
    session
        .on_greeting_sent(&untagged(b"PREAUTH ready"))
        .unwrap();
    session
        .on_command(&command(
            b"SELECT",
            crate::CommandBody::Select {
                mailbox: Bytes::from_static(b"INBOX"),
            },
        ))
        .unwrap();
    assert!(matches!(
        session
            .on_response_sent(&tagged(b"SELECT", Status::Ok))
            .unwrap(),
        ServerSessionEvent::Completed {
            saved_search: SavedSearchUpdate::Reset,
            ..
        }
    ));

    session
        .on_command(&command(
            b"SAVE",
            crate::CommandBody::Search {
                criteria: Bytes::from_static(b"RETURN (MIN MAX SAVE) UNSEEN"),
            },
        ))
        .unwrap();
    assert!(matches!(
        session
            .on_response_sent(&tagged(b"SAVE", Status::Ok))
            .unwrap(),
        ServerSessionEvent::Completed {
            saved_search: SavedSearchUpdate::Replace(SavedSearchScope::MinimumAndMaximum),
            ..
        }
    ));

    assert_eq!(
        session
            .on_response_sent(&untagged(b"OK [UIDVALIDITY 43] changed"))
            .unwrap(),
        ServerSessionEvent::SavedSearchReset
    );
}

#[test]
fn logout_requires_bye_before_ok_and_allows_tag_reuse_after_completion() {
    let mut session = ServerSession::default();
    session.on_greeting_sent(&untagged(b"OK ready")).unwrap();
    session
        .on_command(&command(b"same", crate::CommandBody::Noop))
        .unwrap();
    session
        .on_response_sent(&tagged(b"same", Status::Ok))
        .unwrap();
    session
        .on_command(&command(b"same", crate::CommandBody::Logout))
        .unwrap();
    assert!(
        session
            .on_response_sent(&tagged(b"same", Status::Ok))
            .is_err()
    );
    session
        .on_response_sent(&untagged(b"BYE logging out"))
        .unwrap();
    session
        .on_response_sent(&tagged(b"same", Status::Ok))
        .unwrap();
    assert_eq!(session.state(), SessionState::Logout);
}

#[test]
fn mismatched_tag_and_busy_slot_do_not_destroy_pending_command() {
    let mut session = ServerSession::default();
    session.on_greeting_sent(&untagged(b"OK ready")).unwrap();
    session
        .on_command(&command(b"A1", crate::CommandBody::Noop))
        .unwrap();
    assert!(
        session
            .on_command(&command(b"A2", crate::CommandBody::Capability))
            .is_err()
    );
    assert!(
        session
            .on_response_sent(&tagged(b"A2", Status::Ok))
            .is_err()
    );
    assert_eq!(
        session.pending_tag().map(Bytes::as_ref),
        Some(b"A1".as_slice())
    );
    session
        .on_response_sent(&tagged(b"A1", Status::Ok))
        .unwrap();
}
