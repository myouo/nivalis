use core::time::Duration;

use super::super::IDLE_REISSUE_INTERVAL;
use super::*;

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

fn command(tag: &'static [u8], body: CommandBody) -> Command {
    Command {
        tag: Bytes::from_static(tag),
        body,
    }
}

fn selected_session() -> ClientSession {
    let mut session = ClientSession::default();
    session.on_response(&untagged(b"PREAUTH ready")).unwrap();
    session
        .register_command(&command(
            b"SELECT",
            CommandBody::Select {
                mailbox: Bytes::from_static(b"INBOX"),
            },
        ))
        .unwrap();
    session.on_response(&tagged(b"SELECT", Status::Ok)).unwrap();
    session
}

fn fetch(tag: &'static [u8]) -> Command {
    command(
        tag,
        CommandBody::Fetch {
            sequence_set: crate::SequenceSet::parse(b"1:4").unwrap(),
            items: Bytes::from_static(b"FLAGS"),
        },
    )
}

fn store(tag: &'static [u8]) -> Command {
    command(
        tag,
        CommandBody::Store {
            sequence_set: crate::SequenceSet::parse(b"1:4").unwrap(),
            operation: crate::StoreOperation::Add,
            silent: false,
            flags: Bytes::from_static(b"(\\Seen)"),
        },
    )
}

fn copy(tag: &'static [u8]) -> Command {
    command(
        tag,
        CommandBody::Copy {
            sequence_set: crate::SequenceSet::parse(b"1:4").unwrap(),
            mailbox: Bytes::from_static(b"Archive"),
        },
    )
}

#[test]
fn greeting_login_select_and_close_transition_state() {
    let mut session = ClientSession::default();
    session.on_response(&untagged(b"OK ready")).unwrap();
    assert_eq!(session.state(), SessionState::NotAuthenticated);

    session
        .register_command(&command(
            b"A1",
            CommandBody::Login {
                username: Bytes::from_static(b"user"),
                password: Bytes::from_static(b"pass"),
            },
        ))
        .unwrap();
    session.on_response(&tagged(b"A1", Status::Ok)).unwrap();
    assert_eq!(session.state(), SessionState::Authenticated);

    session
        .register_command(&command(
            b"A2",
            CommandBody::Select {
                mailbox: Bytes::from_static(b"INBOX"),
            },
        ))
        .unwrap();
    session.on_response(&tagged(b"A2", Status::Ok)).unwrap();
    assert_eq!(session.state(), SessionState::Selected { read_only: false });

    session
        .register_command(&command(b"A3", CommandBody::Close))
        .unwrap();
    session.on_response(&tagged(b"A3", Status::Ok)).unwrap();
    assert_eq!(session.state(), SessionState::Authenticated);
}

#[test]
fn failed_reselect_leaves_authenticated_state() {
    let mut session = ClientSession::default();
    session.on_response(&untagged(b"PREAUTH ready")).unwrap();
    let mut capabilities = CapabilitySet::default();
    capabilities.insert(Capability::Enable);
    capabilities.insert(Capability::CondStore);
    session.set_capabilities(capabilities);
    session
        .register_command(&command(
            b"A1",
            CommandBody::Select {
                mailbox: Bytes::from_static(b"missing"),
            },
        ))
        .unwrap();
    session.on_response(&tagged(b"A1", Status::No)).unwrap();
    assert_eq!(session.state(), SessionState::Authenticated);
    assert!(
        session
            .register_command(&command(
                b"A2",
                CommandBody::Enable {
                    capabilities: vec![Bytes::from_static(b"CONDSTORE")],
                },
            ))
            .is_err()
    );
}

#[test]
fn starttls_requires_capability_and_resets_capabilities() {
    let mut session = ClientSession::default();
    session.on_response(&untagged(b"OK ready")).unwrap();
    let starttls = command(b"A1", CommandBody::StartTls);
    assert!(session.register_command(&starttls).is_err());

    session
        .on_response(&untagged(b"CAPABILITY IMAP4rev2 STARTTLS"))
        .unwrap();
    session.register_command(&starttls).unwrap();
    session.on_response(&tagged(b"A1", Status::Ok)).unwrap();
    assert_eq!(session.security(), SecurityState::TlsHandshake);
    assert!(session.capabilities().is_empty());
    assert!(
        session
            .register_command(&command(b"A2", CommandBody::Capability))
            .is_err()
    );
    assert!(
        session
            .on_response(&untagged(b"OK injected plaintext"))
            .is_err()
    );
    session.on_tls_established().unwrap();
    assert_eq!(session.security(), SecurityState::Tls);
}

#[test]
fn rejects_duplicate_tags_and_unmatched_completions() {
    let mut session = ClientSession::default();
    session.on_response(&untagged(b"PREAUTH ready")).unwrap();
    let first = command(b"A1", CommandBody::Noop);
    session.register_command(&first).unwrap();
    assert!(session.register_command(&first).is_err());
    assert!(session.on_response(&tagged(b"A2", Status::Ok)).is_err());
    assert_eq!(session.in_flight_count(), 1);
}

#[test]
fn greeting_capabilities_and_read_only_selection_are_applied() {
    let mut session = ClientSession::default();
    session
        .on_response(&untagged(b"OK [CAPABILITY IMAP4rev2 IDLE] service ready"))
        .unwrap();
    assert!(session.capabilities().contains(&Capability::Idle));
    assert!(
        session
            .register_command(&command(
                b"A1",
                CommandBody::Select {
                    mailbox: Bytes::from_static(b"Archive"),
                },
            ))
            .is_err()
    );

    let mut preauthenticated = ClientSession::default();
    preauthenticated
        .on_response(&untagged(b"PREAUTH ready"))
        .unwrap();
    preauthenticated
        .register_command(&command(
            b"A1",
            CommandBody::Select {
                mailbox: Bytes::from_static(b"Archive"),
            },
        ))
        .unwrap();
    preauthenticated
        .on_response(&Response::Tagged {
            tag: Bytes::from_static(b"A1"),
            status: Status::Ok,
            information: Bytes::from_static(b"[READ-ONLY] selected"),
        })
        .unwrap();
    assert_eq!(
        preauthenticated.state(),
        SessionState::Selected { read_only: true }
    );
}

#[test]
fn idle_requires_capability_continuation_done_and_tagged_completion() {
    assert_eq!(IDLE_REISSUE_INTERVAL, Duration::from_secs(29 * 60));

    let idle = command(b"A2", CommandBody::Idle);
    let mut session = selected_session();
    assert!(session.register_command(&idle).is_err());

    let mut capabilities = CapabilitySet::default();
    capabilities.insert(Capability::Idle);
    session.set_capabilities(capabilities);
    session.register_command(&idle).unwrap();

    assert!(session.on_idle_done(crate::IdleDone).is_err());
    assert!(
        session
            .register_command(&command(b"A3", CommandBody::Noop))
            .is_err()
    );
    assert_eq!(
        session.on_response(&untagged(b"4 EXISTS")).unwrap(),
        SessionEvent::Unsolicited
    );
    assert!(matches!(
        session
            .on_response(&Response::Continuation {
                data: Bytes::from_static(b"idling"),
            })
            .unwrap(),
        SessionEvent::Continuation {
            command: PendingCommand::Idle,
            ..
        }
    ));
    assert!(
        session
            .on_response(&Response::Continuation {
                data: Bytes::from_static(b"duplicate"),
            })
            .is_err()
    );
    assert_eq!(
        session.on_response(&untagged(b"2 EXPUNGE")).unwrap(),
        SessionEvent::Unsolicited
    );
    assert!(session.on_response(&tagged(b"A2", Status::Ok)).is_err());
    assert_eq!(session.in_flight_count(), 1);

    session.on_idle_done(crate::IdleDone).unwrap();
    assert!(session.on_idle_done(crate::IdleDone).is_err());
    assert_eq!(
        session.on_response(&untagged(b"3 EXISTS")).unwrap(),
        SessionEvent::Unsolicited
    );
    assert!(matches!(
        session.on_response(&tagged(b"A2", Status::Ok)).unwrap(),
        SessionEvent::Completed {
            command: PendingCommand::Idle,
            status: Status::Ok,
            ..
        }
    ));
    session
        .register_command(&command(b"A3", CommandBody::Noop))
        .unwrap();
}

#[test]
fn idle_rejection_before_continuation_is_atomic_and_non_idle_continuations_fail() {
    for status in [Status::No, Status::Bad] {
        let mut session = selected_session();
        let mut capabilities = CapabilitySet::default();
        capabilities.insert(Capability::Idle);
        session.set_capabilities(capabilities);
        session
            .register_command(&command(b"IDLE", CommandBody::Idle))
            .unwrap();
        assert!(matches!(
            session.on_response(&tagged(b"IDLE", status)).unwrap(),
            SessionEvent::Completed {
                command: PendingCommand::Idle,
                status: actual,
                ..
            } if actual == status
        ));
        assert_eq!(session.in_flight_count(), 0);
    }

    let mut login = ClientSession::default();
    login.on_response(&untagged(b"OK ready")).unwrap();
    login
        .register_command(&command(
            b"LOGIN",
            CommandBody::Login {
                username: Bytes::from_static(b"user"),
                password: Bytes::from_static(b"pass"),
            },
        ))
        .unwrap();
    assert!(
        login
            .on_response(&Response::Continuation { data: Bytes::new() })
            .is_err()
    );
    assert_eq!(login.in_flight_count(), 1);
}

#[test]
fn enable_is_additive_correlated_and_committed_only_after_ok() {
    let mut session = ClientSession::default();
    session.on_response(&untagged(b"PREAUTH ready")).unwrap();
    let mut advertised = CapabilitySet::default();
    for capability in [
        Capability::Enable,
        Capability::CondStore,
        Capability::Utf8Accept,
    ] {
        advertised.insert(capability);
    }
    session.set_capabilities(advertised);

    assert!(
        session
            .on_response(&untagged(b"ENABLED CONDSTORE"))
            .is_err()
    );
    session
        .register_command(&command(
            b"A1",
            CommandBody::Enable {
                capabilities: vec![Bytes::from_static(b"CONDSTORE")],
            },
        ))
        .unwrap();
    session
        .register_command(&command(
            b"A2",
            CommandBody::Enable {
                capabilities: vec![Bytes::from_static(b"UTF8=ACCEPT")],
            },
        ))
        .unwrap();

    assert!(matches!(
        session.on_response(&untagged(b"ENABLED CONDSTORE")).unwrap(),
        SessionEvent::CapabilitiesEnabled { capabilities }
            if capabilities.contains(&Capability::CondStore)
    ));
    assert!(session.enabled_capabilities().is_empty());
    assert!(
        session
            .on_response(&untagged(b"ENABLED CONDSTORE"))
            .is_err()
    );
    assert_eq!(session.in_flight_count(), 2);

    session.on_response(&tagged(b"A1", Status::Ok)).unwrap();
    assert!(
        session
            .enabled_capabilities()
            .contains(&Capability::CondStore)
    );
    assert!(session.on_response(&tagged(b"A2", Status::Ok)).is_err());
    assert_eq!(session.in_flight_count(), 1);
    session
        .on_response(&untagged(b"ENABLED UTF8=ACCEPT"))
        .unwrap();
    session.on_response(&tagged(b"A2", Status::Ok)).unwrap();
    assert!(
        session
            .enabled_capabilities()
            .contains(&Capability::Utf8Accept)
    );
    assert_eq!(session.enabled_capabilities().len(), 2);
}

#[test]
fn qresync_select_and_vanished_require_successful_enable() {
    let mut session = ClientSession::default();
    session.on_response(&untagged(b"PREAUTH ready")).unwrap();
    let mut advertised = CapabilitySet::default();
    for capability in [
        Capability::Enable,
        Capability::CondStore,
        Capability::QResync,
    ] {
        advertised.insert(capability);
    }
    session.set_capabilities(advertised);

    let select = Command::parse(Bytes::from_static(
        b"S1 SELECT INBOX (QRESYNC (777 12345 1:20))\r\n",
    ))
    .unwrap();
    assert!(session.register_command(&select).is_err());
    assert_eq!(session.in_flight_count(), 0);
    assert!(session.on_response(&untagged(b"VANISHED 1:3")).is_err());

    session
        .register_command(&command(
            b"E1",
            CommandBody::Enable {
                capabilities: vec![Bytes::from_static(b"QRESYNC")],
            },
        ))
        .unwrap();
    session.on_response(&untagged(b"ENABLED QRESYNC")).unwrap();
    session.on_response(&tagged(b"E1", Status::Ok)).unwrap();
    assert!(
        session
            .enabled_capabilities()
            .contains(&Capability::QResync)
    );

    session.register_command(&select).unwrap();
    assert!(matches!(
        session.on_response(&untagged(b"VANISHED (EARLIER) 1:3")),
        Ok(SessionEvent::Unsolicited)
    ));
}

#[test]
fn sort_thread_quota_and_highestmodseq_require_state_and_capabilities() {
    let sort = Command::parse(Bytes::from_static(b"S1 SORT (DATE) UTF-8 ALL\r\n")).unwrap();
    let uid_sort = Command::parse(Bytes::from_static(b"S2 UID SORT (DATE) UTF-8 ALL\r\n")).unwrap();
    let thread = Command::parse(Bytes::from_static(b"T1 THREAD REFERENCES UTF-8 ALL\r\n")).unwrap();

    let mut authenticated = ClientSession::default();
    authenticated
        .on_response(&untagged(b"PREAUTH ready"))
        .unwrap();
    let mut advertised = CapabilitySet::default();
    advertised.insert(Capability::Sort);
    advertised.insert(Capability::Thread {
        algorithm: Bytes::from_static(b"REFERENCES"),
    });
    authenticated.set_capabilities(advertised);
    assert!(authenticated.register_command(&sort).is_err());
    assert!(authenticated.register_command(&thread).is_err());

    let mut selected = selected_session();
    assert!(selected.register_command(&sort).is_err());
    let mut advertised = CapabilitySet::default();
    advertised.insert(Capability::Sort);
    advertised.insert(Capability::Thread {
        algorithm: Bytes::from_static(b"ORDEREDSUBJECT"),
    });
    selected.set_capabilities(advertised.clone());
    selected.register_command(&sort).unwrap();
    selected.on_response(&tagged(b"S1", Status::Ok)).unwrap();
    selected.register_command(&uid_sort).unwrap();
    selected.on_response(&tagged(b"S2", Status::Ok)).unwrap();
    assert!(selected.register_command(&thread).is_err());
    advertised.insert(Capability::Thread {
        algorithm: Bytes::from_static(b"references"),
    });
    selected.set_capabilities(advertised);
    selected.register_command(&thread).unwrap();
    selected.on_response(&tagged(b"T1", Status::Ok)).unwrap();

    let get = Command::parse(Bytes::from_static(b"Q1 GETQUOTA \"\"\r\n")).unwrap();
    let get_root = Command::parse(Bytes::from_static(b"Q2 GETQUOTAROOT INBOX\r\n")).unwrap();
    let set = Command::parse(Bytes::from_static(
        b"Q3 SETQUOTA \"\" (STORAGE 100 MESSAGE 20)\r\n",
    ))
    .unwrap();
    let status = Command::parse(Bytes::from_static(
        b"H1 STATUS INBOX (MESSAGES HIGHESTMODSEQ)\r\n",
    ))
    .unwrap();
    let deleted_storage =
        Command::parse(Bytes::from_static(b"D1 STATUS INBOX (DELETED-STORAGE)\r\n")).unwrap();
    let mut quota = ClientSession::default();
    quota.on_response(&untagged(b"PREAUTH ready")).unwrap();
    assert!(quota.register_command(&get).is_err());
    let mut advertised = CapabilitySet::default();
    advertised.insert(Capability::QuotaResource {
        resource: Bytes::from_static(b"STORAGE"),
    });
    quota.set_capabilities(advertised.clone());
    assert!(quota.register_command(&get).is_err());
    advertised = CapabilitySet::default();
    advertised.insert(Capability::Quota);
    quota.set_capabilities(advertised.clone());
    quota.register_command(&get).unwrap();
    quota.on_response(&tagged(b"Q1", Status::Ok)).unwrap();
    quota.register_command(&get_root).unwrap();
    quota.on_response(&tagged(b"Q2", Status::Ok)).unwrap();
    assert!(quota.register_command(&deleted_storage).is_err());
    advertised.insert(Capability::QuotaResource {
        resource: Bytes::from_static(b"STORAGE"),
    });
    quota.set_capabilities(advertised.clone());
    quota.register_command(&deleted_storage).unwrap();
    quota.on_response(&tagged(b"D1", Status::Ok)).unwrap();
    assert!(quota.register_command(&set).is_err());
    advertised.insert(Capability::QuotaSet);
    quota.set_capabilities(advertised.clone());
    assert!(quota.register_command(&set).is_err());
    advertised.insert(Capability::QuotaResource {
        resource: Bytes::from_static(b"message"),
    });
    quota.set_capabilities(advertised);
    quota.register_command(&set).unwrap();
    quota.on_response(&tagged(b"Q3", Status::Ok)).unwrap();

    assert!(quota.register_command(&status).is_err());
    let mut advertised = quota.capabilities().clone();
    advertised.insert(Capability::CondStore);
    quota.set_capabilities(advertised);
    quota.register_command(&status).unwrap();
}

#[test]
fn enable_failure_and_empty_success_preserve_atomic_state() {
    let mut session = ClientSession::default();
    session.on_response(&untagged(b"PREAUTH ready")).unwrap();
    let mut advertised = CapabilitySet::default();
    advertised.insert(Capability::Enable);
    advertised.insert(Capability::Other {
        token: Bytes::from_static(b"X-OPTION"),
    });
    session.set_capabilities(advertised);

    session
        .register_command(&command(
            b"BAD",
            CommandBody::Enable {
                capabilities: vec![Bytes::from_static(b"X-OPTION")],
            },
        ))
        .unwrap();
    session.on_response(&tagged(b"BAD", Status::Bad)).unwrap();
    assert!(session.enabled_capabilities().is_empty());

    session
        .register_command(&command(
            b"EMPTY",
            CommandBody::Enable {
                capabilities: vec![Bytes::from_static(b"X-OPTION")],
            },
        ))
        .unwrap();
    session.on_response(&untagged(b"ENABLED")).unwrap();
    assert!(session.on_response(&tagged(b"EMPTY", Status::Bad)).is_err());
    assert_eq!(session.in_flight_count(), 1);
    session.on_response(&tagged(b"EMPTY", Status::Ok)).unwrap();
    assert!(session.enabled_capabilities().is_empty());
}

#[test]
fn enable_pipeline_projects_login_and_remembers_successful_selection() {
    let mut session = ClientSession::default();
    session
        .on_response(&untagged(
            b"OK [CAPABILITY IMAP4rev1 ENABLE CONDSTORE] ready",
        ))
        .unwrap();
    session
        .register_command(&command(
            b"LOGIN",
            CommandBody::Login {
                username: Bytes::from_static(b"user"),
                password: Bytes::from_static(b"secret"),
            },
        ))
        .unwrap();
    session
        .register_command(&command(
            b"ENABLE",
            CommandBody::Enable {
                capabilities: vec![Bytes::from_static(b"CONDSTORE")],
            },
        ))
        .unwrap();
    session
        .register_command(&command(
            b"SELECT",
            CommandBody::Select {
                mailbox: Bytes::from_static(b"INBOX"),
            },
        ))
        .unwrap();

    session
        .on_response(&untagged(b"ENABLED CONDSTORE"))
        .unwrap();
    assert!(session.on_response(&tagged(b"ENABLE", Status::Ok)).is_err());
    assert_eq!(session.in_flight_count(), 3);

    session.on_response(&tagged(b"LOGIN", Status::Ok)).unwrap();
    session.on_response(&tagged(b"ENABLE", Status::Ok)).unwrap();
    session.on_response(&tagged(b"SELECT", Status::Ok)).unwrap();
    assert_eq!(session.state(), SessionState::Selected { read_only: false });

    session
        .register_command(&command(b"CLOSE", CommandBody::Unselect))
        .unwrap();
    session.on_response(&tagged(b"CLOSE", Status::Ok)).unwrap();
    assert_eq!(session.state(), SessionState::Authenticated);
    assert!(
        session
            .register_command(&command(
                b"LATE",
                CommandBody::Enable {
                    capabilities: vec![Bytes::from_static(b"CONDSTORE")],
                },
            ))
            .is_err()
    );
}

#[test]
fn failed_projected_login_keeps_later_failures_unauthenticated() {
    let mut session = ClientSession::default();
    session
        .on_response(&untagged(b"OK [CAPABILITY IMAP4rev1 ENABLE] ready"))
        .unwrap();
    session
        .register_command(&command(
            b"LOGIN",
            CommandBody::Login {
                username: Bytes::from_static(b"user"),
                password: Bytes::from_static(b"bad"),
            },
        ))
        .unwrap();
    session
        .register_command(&command(
            b"ENABLE",
            CommandBody::Enable {
                capabilities: vec![Bytes::from_static(b"X-UNKNOWN")],
            },
        ))
        .unwrap();
    session
        .register_command(&command(
            b"SELECT",
            CommandBody::Select {
                mailbox: Bytes::from_static(b"INBOX"),
            },
        ))
        .unwrap();

    session.on_response(&tagged(b"LOGIN", Status::No)).unwrap();
    session
        .on_response(&tagged(b"ENABLE", Status::Bad))
        .unwrap();
    session
        .on_response(&tagged(b"SELECT", Status::Bad))
        .unwrap();
    assert_eq!(session.state(), SessionState::NotAuthenticated);
    assert_eq!(session.in_flight_count(), 0);
}

#[test]
fn closed_cannot_bypass_projected_authentication_and_requires_reselection() {
    let mut projected = ClientSession::default();
    projected
        .on_response(&untagged(
            b"OK [CAPABILITY IMAP4rev1 ENABLE CONDSTORE] ready",
        ))
        .unwrap();
    projected
        .register_command(&command(
            b"L",
            CommandBody::Login {
                username: Bytes::from_static(b"user"),
                password: Bytes::from_static(b"secret"),
            },
        ))
        .unwrap();
    projected
        .register_command(&command(
            b"E",
            CommandBody::Enable {
                capabilities: vec![Bytes::from_static(b"CONDSTORE")],
            },
        ))
        .unwrap();
    projected
        .register_command(&command(
            b"S",
            CommandBody::Select {
                mailbox: Bytes::from_static(b"INBOX"),
            },
        ))
        .unwrap();
    projected
        .on_response(&untagged(b"ENABLED CONDSTORE"))
        .unwrap();

    assert!(projected.on_response(&untagged(b"OK [CLOSED] ")).is_err());
    assert_eq!(projected.state(), SessionState::NotAuthenticated);
    assert!(projected.on_response(&tagged(b"E", Status::Ok)).is_err());
    assert_eq!(projected.in_flight_count(), 3);

    let mut selected = selected_session();
    selected
        .register_command(&command(
            b"R",
            CommandBody::Select {
                mailbox: Bytes::from_static(b"other"),
            },
        ))
        .unwrap();
    assert_eq!(
        selected.on_response(&untagged(b"OK [CLOSED] ")).unwrap(),
        SessionEvent::MailboxClosed
    );
    assert_eq!(selected.state(), SessionState::Authenticated);
    selected.on_response(&tagged(b"R", Status::Ok)).unwrap();
    assert_eq!(
        selected.state(),
        SessionState::Selected { read_only: false }
    );
}

#[test]
fn pipelined_enabled_responses_match_compatible_commands_not_wire_order() {
    let mut session = ClientSession::default();
    session.on_response(&untagged(b"PREAUTH ready")).unwrap();
    let mut advertised = CapabilitySet::default();
    for capability in [
        Capability::Enable,
        Capability::CondStore,
        Capability::Utf8Accept,
    ] {
        advertised.insert(capability);
    }
    session.set_capabilities(advertised);
    for (tag, capability) in [
        (b"A1".as_slice(), b"CONDSTORE".as_slice()),
        (b"A2".as_slice(), b"UTF8=ACCEPT".as_slice()),
    ] {
        session
            .register_command(&Command {
                tag: Bytes::copy_from_slice(tag),
                body: CommandBody::Enable {
                    capabilities: vec![Bytes::copy_from_slice(capability)],
                },
            })
            .unwrap();
    }

    session
        .on_response(&untagged(b"ENABLED UTF8=ACCEPT"))
        .unwrap();
    session.on_response(&tagged(b"A2", Status::Ok)).unwrap();
    session
        .on_response(&untagged(b"ENABLED CONDSTORE"))
        .unwrap();
    session.on_response(&tagged(b"A1", Status::Ok)).unwrap();
    assert_eq!(session.enabled_capabilities().len(), 2);
}

#[test]
fn enabled_response_cannot_be_claimed_by_a_later_request() {
    let mut session = ClientSession::default();
    session.on_response(&untagged(b"PREAUTH ready")).unwrap();
    let mut advertised = CapabilitySet::default();
    advertised.insert(Capability::Enable);
    advertised.insert(Capability::CondStore);
    session.set_capabilities(advertised);

    for tag in [b"A1".as_slice(), b"A2"] {
        if tag == b"A2" {
            session
                .on_response(&untagged(b"ENABLED CONDSTORE"))
                .unwrap();
        }
        session
            .register_command(&command(
                tag,
                CommandBody::Enable {
                    capabilities: vec![Bytes::from_static(b"CONDSTORE")],
                },
            ))
            .unwrap();
    }

    assert!(session.on_response(&tagged(b"A2", Status::Ok)).is_err());
    assert_eq!(session.in_flight_count(), 2);
    assert!(session.enabled_capabilities().is_empty());
    session.on_response(&tagged(b"A1", Status::Ok)).unwrap();
    session
        .on_response(&untagged(b"ENABLED CONDSTORE"))
        .unwrap();
    session.on_response(&tagged(b"A2", Status::Ok)).unwrap();
    assert!(
        session
            .enabled_capabilities()
            .contains(&Capability::CondStore)
    );
}

#[test]
fn concurrent_enable_requests_have_a_fixed_matching_budget() {
    let mut session = ClientSession::new(64);
    session.on_response(&untagged(b"PREAUTH ready")).unwrap();
    let mut advertised = CapabilitySet::default();
    advertised.insert(Capability::Enable);
    advertised.insert(Capability::CondStore);
    session.set_capabilities(advertised);

    for index in 0..MAX_IN_FLIGHT_ENABLE {
        session
            .register_command(&Command {
                tag: Bytes::from(format!("A{index}")),
                body: CommandBody::Enable {
                    capabilities: vec![Bytes::from_static(b"CONDSTORE")],
                },
            })
            .unwrap();
    }
    assert_eq!(session.in_flight_count(), MAX_IN_FLIGHT_ENABLE);
    assert!(
        session
            .register_command(&command(
                b"OVERFLOW",
                CommandBody::Enable {
                    capabilities: vec![Bytes::from_static(b"CONDSTORE")],
                },
            ))
            .is_err()
    );
    assert_eq!(session.in_flight_count(), MAX_IN_FLIGHT_ENABLE);
}

#[test]
fn correlates_esearch_with_search_and_uid_search() {
    let mut search = selected_session();
    assert_eq!(
        search
            .register_command(&command(
                b"A1",
                CommandBody::Search {
                    criteria: Bytes::from_static(b"RETURN (COUNT) UNSEEN"),
                },
            ))
            .unwrap(),
        PendingCommand::Search
    );
    let event = search
        .on_response(&untagged(b"ESEARCH (TAG \"A1\") COUNT 3"))
        .unwrap();
    assert!(matches!(
        event,
        SessionEvent::SearchResults { response }
            if response.tag().unwrap().decoded().as_ref() == b"A1"
                && !response.is_uid()
                && response.count() == Some(3)
    ));
    assert_eq!(search.in_flight_count(), 1);
    search.on_response(&tagged(b"A1", Status::Ok)).unwrap();

    let mut uid = selected_session();
    assert_eq!(
        uid.register_command(&command(
            b"U1",
            CommandBody::Uid {
                command: Bytes::from_static(b"SEARCH"),
                arguments: Bytes::from_static(b"RETURN (ALL) UNSEEN"),
            },
        ))
        .unwrap(),
        PendingCommand::UidSearch
    );
    assert!(matches!(
        uid.on_response(&untagged(b"ESEARCH (TAG U1) UID ALL 4:9"))
            .unwrap(),
        SessionEvent::SearchResults { response }
            if response.is_uid() && response.all().unwrap().as_bytes() == b"4:9"
    ));
}

#[test]
fn rejects_mismatched_esearch_correlators_without_consuming_command() {
    let mut session = selected_session();
    session
        .register_command(&command(
            b"A1",
            CommandBody::Search {
                criteria: Bytes::from_static(b"UNSEEN"),
            },
        ))
        .unwrap();

    assert!(
        session
            .on_response(&untagged(b"ESEARCH (TAG A2) COUNT 1"))
            .is_err()
    );
    assert!(
        session
            .on_response(&untagged(b"ESEARCH (TAG A1) UID COUNT 1"))
            .is_err()
    );
    assert_eq!(session.in_flight_count(), 1);
    session.on_response(&tagged(b"A1", Status::Ok)).unwrap();
}

#[test]
fn exposes_uncorrelated_esearch_as_unsolicited_typed_data() {
    let mut session = selected_session();
    assert!(matches!(
        session
            .on_response(&untagged(b"ESEARCH COUNT 0"))
            .unwrap(),
        SessionEvent::SearchResults { response } if response.count() == Some(0)
    ));
}

#[test]
fn correlates_childinfo_across_pipelined_list_commands() {
    let mut session = ClientSession::default();
    session.on_response(&untagged(b"PREAUTH ready")).unwrap();
    assert_eq!(
        session
            .register_command(&command(
                b"SUBSCRIBED",
                CommandBody::List {
                    arguments: Bytes::from_static(b"(RECURSIVEMATCH SUBSCRIBED) \"\" *"),
                },
            ))
            .unwrap(),
        PendingCommand::List
    );
    session
        .register_command(&command(
            b"EXTENSION",
            CommandBody::List {
                arguments: Bytes::from_static(b"(X-FLAG RECURSIVEMATCH) \"\" *"),
            },
        ))
        .unwrap();

    let event = session
        .on_response(&untagged(
            b"LIST () \"/\" parent (\"CHILDINFO\" (\"subscribed\"))",
        ))
        .unwrap();
    assert!(matches!(
        event,
        SessionEvent::ListData {
            response,
            correlation: ListCorrelation::Matched { tag },
        } if tag == b"SUBSCRIBED".as_slice()
            && response.mailbox().decoded().as_ref() == b"parent"
    ));

    assert!(matches!(
        session
            .on_response(&untagged(b"LIST (\\Subscribed) \"/\" child"))
            .unwrap(),
        SessionEvent::ListData {
            correlation: ListCorrelation::Unspecified,
            ..
        }
    ));
}

#[test]
fn reports_ambiguous_childinfo_for_equivalent_list_criteria() {
    let mut session = ClientSession::default();
    session.on_response(&untagged(b"PREAUTH ready")).unwrap();
    for tag in [b"A1".as_slice(), b"A2"] {
        session
            .register_command(&Command {
                tag: Bytes::copy_from_slice(tag),
                body: CommandBody::List {
                    arguments: Bytes::from_static(b"(SUBSCRIBED RECURSIVEMATCH) \"\" *"),
                },
            })
            .unwrap();
    }

    assert!(matches!(
        session
            .on_response(&untagged(
                b"LIST () \"/\" parent (CHILDINFO (\"SUBSCRIBED\"))",
            ))
            .unwrap(),
        SessionEvent::ListData {
            correlation: ListCorrelation::Ambiguous,
            ..
        }
    ));
}

#[test]
fn rejects_unmatched_childinfo_without_consuming_list_commands() {
    let mut session = ClientSession::default();
    session.on_response(&untagged(b"PREAUTH ready")).unwrap();
    session
        .register_command(&command(
            b"A1",
            CommandBody::List {
                arguments: Bytes::from_static(b"\"\" *"),
            },
        ))
        .unwrap();

    assert!(
        session
            .on_response(&untagged(
                b"LIST () \"/\" parent (CHILDINFO (\"SUBSCRIBED\"))",
            ))
            .is_err()
    );
    assert_eq!(session.in_flight_count(), 1);
    session.on_response(&tagged(b"A1", Status::Ok)).unwrap();

    assert!(
        session
            .on_response(&untagged(
                b"LIST () \"/\" parent (CHILDINFO (\"SUBSCRIBED\"))",
            ))
            .is_err()
    );
    assert!(matches!(
        session
            .on_response(&untagged(b"LIST () \"/\" unsolicited"))
            .unwrap(),
        SessionEvent::ListData {
            correlation: ListCorrelation::Unspecified,
            ..
        }
    ));
}

#[test]
fn rejects_malformed_manual_list_without_registering_it() {
    let mut session = ClientSession::default();
    session.on_response(&untagged(b"PREAUTH ready")).unwrap();
    assert!(
        session
            .register_command(&command(
                b"A1",
                CommandBody::List {
                    arguments: Bytes::from_static(b"(RECURSIVEMATCH) \"\" *"),
                },
            ))
            .is_err()
    );
    assert_eq!(session.in_flight_count(), 0);
}

#[test]
fn accepts_rfc9051_unambiguous_pipeline_examples() {
    let mut session = selected_session();
    session.register_command(&fetch(b"A1")).unwrap();
    session.register_command(&store(b"A2")).unwrap();
    session
        .register_command(&command(
            b"A3",
            CommandBody::Search {
                criteria: Bytes::from_static(b"UNSEEN"),
            },
        ))
        .unwrap();
    session
        .register_command(&command(b"A4", CommandBody::Noop))
        .unwrap();

    let mut second = selected_session();
    second.register_command(&store(b"B1")).unwrap();
    second.register_command(&copy(b"B2")).unwrap();
    second
        .register_command(&command(b"B3", CommandBody::Expunge))
        .unwrap();
}

#[test]
fn rejects_rfc9051_ambiguous_sequence_number_pipelines() {
    let mut fetch_noop_store = selected_session();
    fetch_noop_store.register_command(&fetch(b"A1")).unwrap();
    fetch_noop_store
        .register_command(&command(b"A2", CommandBody::Noop))
        .unwrap();
    assert!(fetch_noop_store.register_command(&store(b"A3")).is_err());

    let mut store_copy_fetch = selected_session();
    store_copy_fetch.register_command(&store(b"B1")).unwrap();
    store_copy_fetch.register_command(&copy(b"B2")).unwrap();
    assert!(store_copy_fetch.register_command(&fetch(b"B3")).is_err());

    let mut copy_copy = selected_session();
    copy_copy.register_command(&copy(b"C1")).unwrap();
    assert!(copy_copy.register_command(&copy(b"C2")).is_err());
}

#[test]
fn uid_and_state_changes_are_pipeline_barriers() {
    let mut uid = selected_session();
    uid.register_command(&command(
        b"A1",
        CommandBody::Uid {
            command: Bytes::from_static(b"FETCH"),
            arguments: Bytes::from_static(b"1:4 FLAGS"),
        },
    ))
    .unwrap();
    assert!(uid.register_command(&fetch(b"A2")).is_err());

    uid.register_command(&command(
        b"A3",
        CommandBody::Uid {
            command: Bytes::from_static(b"SEARCH"),
            arguments: Bytes::from_static(b"UNSEEN"),
        },
    ))
    .unwrap();
    assert!(
        uid.register_command(&command(
            b"A4",
            CommandBody::Uid {
                command: Bytes::from_static(b"SEARCH"),
                arguments: Bytes::from_static(b"1:4"),
            },
        ))
        .is_err()
    );

    let mut reselect = selected_session();
    reselect
        .register_command(&command(
            b"B1",
            CommandBody::Select {
                mailbox: Bytes::from_static(b"Archive"),
            },
        ))
        .unwrap();
    assert!(
        reselect
            .register_command(&command(b"B2", CommandBody::Noop))
            .is_err()
    );
}

#[test]
fn completed_barrier_releases_later_sequence_commands() {
    let mut session = selected_session();
    session
        .register_command(&command(b"A1", CommandBody::Noop))
        .unwrap();
    assert!(session.register_command(&fetch(b"A2")).is_err());
    session.on_response(&tagged(b"A1", Status::Ok)).unwrap();
    session.register_command(&fetch(b"A2")).unwrap();
}

#[test]
fn search_only_uses_sequence_barrier_for_numeric_criteria() {
    let mut session = selected_session();
    session
        .register_command(&command(b"A1", CommandBody::Noop))
        .unwrap();
    session
        .register_command(&command(
            b"A2",
            CommandBody::Search {
                criteria: Bytes::from_static(b"UNSEEN"),
            },
        ))
        .unwrap();
    assert!(
        session
            .register_command(&command(
                b"A3",
                CommandBody::Search {
                    criteria: Bytes::from_static(b"OR UNSEEN 1:4"),
                },
            ))
            .is_err()
    );
}

#[test]
fn saved_search_pipeline_enforces_only_real_data_dependencies() {
    let mut session = selected_session();
    session
        .register_command(&command(
            b"SAVE",
            CommandBody::Search {
                criteria: Bytes::from_static(b"RETURN (SAVE) UNSEEN"),
            },
        ))
        .unwrap();
    session
        .register_command(&command(
            b"INDEPENDENT",
            CommandBody::Search {
                criteria: Bytes::from_static(b"RETURN (ALL) SEEN"),
            },
        ))
        .unwrap();
    session
        .register_command(&command(
            b"READ",
            CommandBody::Fetch {
                sequence_set: crate::SequenceSet::parse(b"$").unwrap(),
                items: Bytes::from_static(b"FLAGS"),
            },
        ))
        .unwrap();

    assert!(matches!(
        session
            .on_response(&tagged(b"INDEPENDENT", Status::Ok))
            .unwrap(),
        SessionEvent::Completed {
            saved_search: SavedSearchUpdate::Unchanged,
            ..
        }
    ));
    assert!(session.on_response(&tagged(b"READ", Status::Ok)).is_err());
    assert_eq!(session.in_flight_count(), 2);
    assert!(matches!(
        session.on_response(&tagged(b"SAVE", Status::Ok)).unwrap(),
        SessionEvent::Completed {
            saved_search: SavedSearchUpdate::Replace(SavedSearchScope::All),
            ..
        }
    ));
    session.on_response(&tagged(b"READ", Status::Ok)).unwrap();
}

#[test]
fn saved_search_reads_can_reorder_but_later_write_waits_for_all_reads() {
    let mut session = selected_session();
    for tag in [b"R1".as_slice(), b"R2"] {
        session
            .register_command(&Command {
                tag: Bytes::copy_from_slice(tag),
                body: CommandBody::Uid {
                    command: Bytes::from_static(b"FETCH"),
                    arguments: Bytes::from_static(b"$ FLAGS"),
                },
            })
            .unwrap();
    }
    session
        .register_command(&command(
            b"SAVE",
            CommandBody::Search {
                criteria: Bytes::from_static(b"RETURN (SAVE MIN MAX) ALL"),
            },
        ))
        .unwrap();

    session.on_response(&tagged(b"R2", Status::Ok)).unwrap();
    assert!(session.on_response(&tagged(b"SAVE", Status::Ok)).is_err());
    session.on_response(&tagged(b"R1", Status::Ok)).unwrap();
    assert!(matches!(
        session.on_response(&tagged(b"SAVE", Status::Ok)).unwrap(),
        SessionEvent::Completed {
            saved_search: SavedSearchUpdate::Replace(SavedSearchScope::MinimumAndMaximum),
            ..
        }
    ));
}

#[test]
fn failed_saved_search_resets_on_no_but_not_bad() {
    for (tag, status, expected) in [
        (b"NO".as_slice(), Status::No, SavedSearchUpdate::Reset),
        (b"BAD".as_slice(), Status::Bad, SavedSearchUpdate::Unchanged),
    ] {
        let mut session = selected_session();
        session
            .register_command(&Command {
                tag: Bytes::copy_from_slice(tag),
                body: CommandBody::Search {
                    criteria: Bytes::from_static(b"RETURN (SAVE) UNSEEN"),
                },
            })
            .unwrap();
        assert!(matches!(
            session
                .on_response(&Response::Tagged {
                    tag: Bytes::copy_from_slice(tag),
                    status,
                    information: Bytes::new(),
                })
                .unwrap(),
            SessionEvent::Completed { saved_search, .. } if saved_search == expected
        ));
    }
}

#[test]
fn uidvalidity_change_reports_saved_search_reset() {
    let mut session = selected_session();
    assert_eq!(
        session
            .on_response(&untagged(b"OK [UIDVALIDITY 42] changed"))
            .unwrap(),
        SessionEvent::SavedSearchReset
    );

    session
        .register_command(&command(
            b"RESELECT",
            CommandBody::Select {
                mailbox: Bytes::from_static(b"Archive"),
            },
        ))
        .unwrap();
    assert_eq!(
        session
            .on_response(&untagged(b"OK [UIDVALIDITY 99] selecting"))
            .unwrap(),
        SessionEvent::Unsolicited
    );
    assert!(matches!(
        session
            .on_response(&tagged(b"RESELECT", Status::Ok))
            .unwrap(),
        SessionEvent::Completed {
            saved_search: SavedSearchUpdate::Reset,
            ..
        }
    ));
}
