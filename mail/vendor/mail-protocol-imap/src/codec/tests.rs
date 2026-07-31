use super::*;

#[test]
fn waits_for_fragmented_command() {
    let mut decoder = CommandDecoder::default();
    let mut input = BytesMut::from(&b"A1 NO"[..]);
    assert_eq!(
        decoder.decode(&mut input).unwrap(),
        DecodeStatus::Incomplete
    );
    input.extend_from_slice(b"OP\r\n");
    let command = match decoder.decode(&mut input).unwrap() {
        DecodeStatus::Complete(command) => command,
        DecodeStatus::Incomplete => panic!("command should be complete"),
    };
    assert_eq!(command.tag, Bytes::from_static(b"A1"));
    assert_eq!(command.body, CommandBody::Noop);
    assert!(input.is_empty());
}

#[test]
fn literal_crlf_does_not_end_frame() {
    let mut decoder = CommandDecoder::default();
    let mut input = BytesMut::from(&b"A1 XTEST {4}\r\na\r\nb\r\n"[..]);
    let command = match decoder.decode(&mut input).unwrap() {
        DecodeStatus::Complete(command) => command,
        DecodeStatus::Incomplete => panic!("literal command should be complete"),
    };
    match command.body {
        CommandBody::Raw { name, arguments } => {
            assert_eq!(name.as_ref(), b"XTEST");
            assert_eq!(arguments.as_ref(), b"{4}\r\na\r\nb");
        }
        other => panic!("unexpected body: {other:?}"),
    }
}

#[test]
fn literal_marker_must_be_unquoted_and_token_delimited() {
    let valid = b"A1 XTEST \"value {3}\"\r\n";
    let mut input = BytesMut::from(valid.as_slice());
    let DecodeStatus::Complete(command) = CommandDecoder::default().decode(&mut input).unwrap()
    else {
        panic!("quoted raw command expected");
    };
    let mut encoded = BytesMut::new();
    CommandEncoder.encode(&command, &mut encoded).unwrap();
    assert_eq!(encoded.as_ref(), valid);

    for invalid in [
        &b"A1 XTEST \"unterminated {3}\r\n"[..],
        &b"A1 XTEST atom{3}\r\n"[..],
        &b"A1 XTEST ~~{3}\r\n"[..],
    ] {
        let mut input = BytesMut::from(invalid);
        let error = CommandDecoder::default().decode(&mut input).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidSyntax);
        assert_eq!(input.as_ref(), invalid);
    }
}

#[test]
fn command_round_trip_preserves_literal_trailing_whitespace() {
    let original = b"A1 APPEND INBOX {2}\r\n \t\r\n";
    let mut input = BytesMut::from(original.as_slice());
    let DecodeStatus::Complete(command) = CommandDecoder::default().decode(&mut input).unwrap()
    else {
        panic!("complete APPEND command expected");
    };
    assert_eq!(
        command.body,
        CommandBody::Append {
            mailbox: Bytes::from_static(b"INBOX"),
            arguments: Bytes::from_static(b"{2}\r\n \t"),
        }
    );

    let mut encoded = BytesMut::new();
    CommandEncoder.encode(&command, &mut encoded).unwrap();
    assert_eq!(encoded.as_ref(), original);
}

#[test]
fn parses_quoted_login() {
    let mut decoder = CommandDecoder::default();
    let mut input = BytesMut::from(&b"a LOGIN \"user name\" \"p\\\"w\"\r\n"[..]);
    let DecodeStatus::Complete(command) = decoder.decode(&mut input).unwrap() else {
        panic!("complete command expected");
    };
    assert_eq!(
        command.body,
        CommandBody::Login {
            username: Bytes::from_static(b"\"user name\""),
            password: Bytes::from_static(b"\"p\\\"w\"")
        }
    );
}

#[test]
fn response_round_trip() {
    let response = Response::Tagged {
        tag: Bytes::from_static(b"A1"),
        status: Status::Ok,
        information: Bytes::from_static(b"done"),
    };
    let mut wire = BytesMut::new();
    ResponseEncoder.encode(&response, &mut wire).unwrap();
    let DecodeStatus::Complete(decoded) = ResponseDecoder::default().decode(&mut wire).unwrap()
    else {
        panic!("complete response expected");
    };
    assert_eq!(decoded, response);
}

#[test]
fn response_decoder_enforces_status_separators_and_semantics_atomically() {
    for valid in [
        b"A1 OK \r\n".as_slice(),
        b"A1 OK\r\n",
        b"* OK \r\n",
        b"* OK\r\n",
        b"* OK [UNSEEN 3212]\r\n",
        b"A2 OK [READ-ONLY]\r\n",
        b"* OK  [ALERT] is plain text\r\n",
    ] {
        let mut input = BytesMut::from(valid);
        assert!(matches!(
            ResponseDecoder::default().decode(&mut input).unwrap(),
            DecodeStatus::Complete(_)
        ));
        assert!(input.is_empty());
    }

    for invalid in [
        b"A1\tOK done\r\n".as_slice(),
        b"A1 OK\ttext\r\n",
        b"* ID  NIL\r\n",
        b"* CAPABILITY IDLE\r\n",
        b"* OK [UIDNEXT 0] bad\r\n",
    ] {
        let mut input = BytesMut::from(invalid);
        assert!(ResponseDecoder::default().decode(&mut input).is_err());
        assert_eq!(input.as_ref(), invalid);
    }

    let empty = Response::Tagged {
        tag: Bytes::from_static(b"A1"),
        status: Status::Ok,
        information: Bytes::new(),
    };
    let mut output = BytesMut::new();
    ResponseEncoder.encode(&empty, &mut output).unwrap();
    assert_eq!(output.as_ref(), b"A1 OK \r\n");

    let omitted_text_code = Response::Tagged {
        tag: Bytes::from_static(b"A2"),
        status: Status::No,
        information: Bytes::from_static(b"[ALERT]"),
    };
    let mut output = BytesMut::new();
    ResponseEncoder
        .encode(&omitted_text_code, &mut output)
        .unwrap();
    assert_eq!(output.as_ref(), b"A2 NO [ALERT]\r\n");
}

#[test]
fn response_status_text_cannot_be_reclassified_as_a_literal_or_inject_frames() {
    let first = b"* OK done {1}\r\n";
    let second = b"* 1 EXISTS\r\n";
    let mut stream = BytesMut::new();
    stream.extend_from_slice(first);
    stream.extend_from_slice(second);

    let DecodeStatus::Complete(response) = ResponseDecoder::default().decode(&mut stream).unwrap()
    else {
        panic!("status response should be complete");
    };
    assert_eq!(
        response,
        Response::Untagged {
            data: Bytes::from_static(b"OK done {1}"),
        }
    );
    assert_eq!(stream.as_ref(), second);
    assert!(matches!(
        ResponseDecoder::default().decode(&mut stream).unwrap(),
        DecodeStatus::Complete(Response::Untagged { data }) if data.as_ref() == b"1 EXISTS"
    ));

    for response in [
        Response::Tagged {
            tag: Bytes::from_static(b"A1"),
            status: Status::Ok,
            information: Bytes::from_static(b"done {1}\r\n* BYE"),
        },
        Response::Untagged {
            data: Bytes::from_static(b"OK done {1}\r\n* BYE"),
        },
    ] {
        let mut output = BytesMut::from(&b"prefix"[..]);
        assert!(ResponseEncoder.encode(&response, &mut output).is_err());
        assert_eq!(output.as_ref(), b"prefix");
    }
}

#[test]
fn response_lines_reject_nul_and_invalid_utf8_but_literals_preserve_octets() {
    for invalid in [b"* OK \0\r\n".as_slice(), b"+ \0\r\n", b"* OK \xff\r\n"] {
        let mut input = BytesMut::from(invalid);
        assert!(ResponseDecoder::default().decode(&mut input).is_err());
        assert_eq!(input.as_ref(), invalid);
    }

    let mut utf8 = BytesMut::from(b"* OK \xc3\xa9\r\n".as_slice());
    assert!(matches!(
        ResponseDecoder::default().decode(&mut utf8).unwrap(),
        DecodeStatus::Complete(_)
    ));
    assert!(utf8.is_empty());

    let mut decoder = ResponseDecoder::default();
    let mut fragmented_utf8 = BytesMut::from(b"* OK \xc3".as_slice());
    assert_eq!(
        decoder.decode(&mut fragmented_utf8).unwrap(),
        DecodeStatus::Incomplete
    );
    assert_eq!(fragmented_utf8.as_ref(), b"* OK \xc3");
    fragmented_utf8.extend_from_slice(b"\xa9\r\n");
    assert!(matches!(
        decoder.decode(&mut fragmented_utf8).unwrap(),
        DecodeStatus::Complete(_)
    ));
    assert!(fragmented_utf8.is_empty());

    let valid = b"* 1 FETCH (BINARY[] ~{1}\r\n\0)\r\n";
    let mut input = BytesMut::from(valid.as_slice());
    assert!(matches!(
        ResponseDecoder::default().decode(&mut input).unwrap(),
        DecodeStatus::Complete(_)
    ));
    assert!(input.is_empty());

    let ordinary_literal = b"* 1 FETCH (BODY[] {1}\r\n\xff)\r\n";
    let mut input = BytesMut::from(ordinary_literal.as_slice());
    assert!(matches!(
        ResponseDecoder::default().decode(&mut input).unwrap(),
        DecodeStatus::Complete(_)
    ));
    assert!(input.is_empty());

    let response = Response::Continuation {
        data: Bytes::from_static(b"\0"),
    };
    let mut output = BytesMut::from(&b"prefix"[..]);
    assert!(ResponseEncoder.encode(&response, &mut output).is_err());
    assert_eq!(output.as_ref(), b"prefix");

    let response = Response::Continuation {
        data: Bytes::from_static(b"\xff"),
    };
    assert!(ResponseEncoder.encode(&response, &mut output).is_err());
    assert_eq!(output.as_ref(), b"prefix");
}

#[test]
fn owned_complete_response_parser_is_exact_zero_copy_and_limit_aware() {
    let wire = Bytes::from_static(b"* 7 FETCH (BODY[] {3}\r\nabc UID 9)\r\n");
    let pointer = wire.as_ptr();
    let response = Response::parse(wire).unwrap();
    let Response::Untagged { data } = &response else {
        panic!("untagged response expected");
    };
    assert_eq!(data.as_ptr(), pointer.wrapping_add(2));
    assert!(matches!(
        parse_untagged(&response).unwrap(),
        Some(crate::UntaggedData::Fetch { sequence: 7, .. })
    ));

    for invalid in [
        b"* 1 EXISTS".as_slice(),
        b"* 1 EXISTS\r\n* 2 EXISTS\r\n",
        b"* 1 FETCH (UID 1)\n",
    ] {
        assert!(Response::parse(Bytes::copy_from_slice(invalid)).is_err());
    }

    let limits = Limits::default().with_max_literal_len(2);
    assert!(
        Response::parse_with_limits(
            Bytes::from_static(b"* 7 FETCH (BODY[] {3}\r\nabc)\r\n"),
            limits,
        )
        .is_err()
    );
}

#[test]
fn response_round_trip_preserves_literal_trailing_whitespace() {
    let original = b"* X-LITERAL {2}\r\n \t\r\n";
    let mut input = BytesMut::from(original.as_slice());
    let DecodeStatus::Complete(response) = ResponseDecoder::default().decode(&mut input).unwrap()
    else {
        panic!("complete response expected");
    };
    assert_eq!(
        response,
        Response::Untagged {
            data: Bytes::from_static(b"X-LITERAL {2}\r\n \t"),
        }
    );

    let mut encoded = BytesMut::new();
    ResponseEncoder.encode(&response, &mut encoded).unwrap();
    assert_eq!(encoded.as_ref(), original);
}

#[test]
fn response_encoder_validates_fetch_semantics_atomically() {
    let valid = Response::Untagged {
        data: Bytes::from_static(
            b"4294967295 FETCH (UID 4294967295 BODY[1]<7> {3}\r\nabc MODSEQ (01))",
        ),
    };
    let mut encoded = BytesMut::from(&b"prefix"[..]);
    ResponseEncoder.encode(&valid, &mut encoded).unwrap();
    assert_eq!(
        encoded.as_ref(),
        b"prefix* 4294967295 FETCH (UID 4294967295 BODY[1]<7> {3}\r\nabc MODSEQ (01))\r\n"
    );

    for invalid in [
        &b"0 FETCH (UID 1)"[..],
        b"1 FETCH (UID 0)",
        b"1 FETCH (BODY[01] NIL)",
        b"1 FETCH (BODYSTRUCTURE ((\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 1 1))",
        b"SORT 0",
        b"THREAD ((1))",
        b"QUOTA \"\" (STORAGE 1)",
        b"QUOTAROOT INBOX ",
    ] {
        let response = Response::Untagged {
            data: Bytes::copy_from_slice(invalid),
        };
        let mut destination = BytesMut::from(&b"prefix"[..]);
        assert!(ResponseEncoder.encode(&response, &mut destination).is_err());
        assert_eq!(destination.as_ref(), b"prefix");
    }
}

#[test]
fn empty_server_continuation_uses_required_space() {
    let response = Response::Continuation { data: Bytes::new() };
    let mut wire = BytesMut::new();
    ResponseEncoder.encode(&response, &mut wire).unwrap();
    assert_eq!(wire.as_ref(), b"+ \r\n");
    let DecodeStatus::Complete(decoded) = ResponseDecoder::default().decode(&mut wire).unwrap()
    else {
        panic!("complete continuation expected");
    };
    assert_eq!(decoded, response);

    let invalid = b"+\r\n";
    let mut input = BytesMut::from(invalid.as_slice());
    let error = ResponseDecoder::default().decode(&mut input).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidSyntax);
    assert_eq!(input.as_ref(), invalid);
}

#[test]
fn rejects_oversized_literal_without_buffering_it() {
    let limits = Limits::default().with_max_literal_len(3);
    let mut decoder = CommandDecoder::new(limits);
    let mut input = BytesMut::from(&b"A1 X {4}\r\n"[..]);
    let error = decoder.decode(&mut input).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::LiteralTooLarge);
    assert_eq!(input.as_ref(), b"A1 X {4}\r\n");
}

#[test]
fn parses_login_literals_as_typed_astrings() {
    let wire = b"A1 LOGIN {4}\r\nuser {8}\r\np\r\n word\r\n";
    let mut input = BytesMut::from(wire.as_slice());
    let DecodeStatus::Complete(command) = CommandDecoder::default().decode(&mut input).unwrap()
    else {
        panic!("complete literal command expected");
    };
    assert_eq!(
        command.body,
        CommandBody::Login {
            username: Bytes::from_static(b"{4}\r\nuser"),
            password: Bytes::from_static(b"{8}\r\np\r\n word"),
        }
    );
    let mut encoded = BytesMut::new();
    CommandEncoder.encode(&command, &mut encoded).unwrap();
    assert_eq!(encoded.as_ref(), wire);
}

#[test]
fn parses_literal_mailboxes_without_scanning_their_payload_as_syntax() {
    let commands: &[(&[u8], CommandBody)] = &[
        (
            b"A1 SELECT {5}\r\nINBOX\r\n",
            CommandBody::Select {
                mailbox: Bytes::from_static(b"{5}\r\nINBOX"),
            },
        ),
        (
            b"A2 RENAME {3}\r\nOld {3}\r\nNew\r\n",
            CommandBody::Rename {
                from: Bytes::from_static(b"{3}\r\nOld"),
                to: Bytes::from_static(b"{3}\r\nNew"),
            },
        ),
        (
            b"A3 COPY 1:2 {7}\r\nArchive\r\n",
            CommandBody::Copy {
                sequence_set: SequenceSet::parse(b"1:2").unwrap(),
                mailbox: Bytes::from_static(b"{7}\r\nArchive"),
            },
        ),
        (
            b"A4 APPEND {5}\r\nINBOX {4}\r\ntest\r\n",
            CommandBody::Append {
                mailbox: Bytes::from_static(b"{5}\r\nINBOX"),
                arguments: Bytes::from_static(b"{4}\r\ntest"),
            },
        ),
    ];

    for (wire, expected) in commands {
        let mut input = BytesMut::from(*wire);
        let DecodeStatus::Complete(command) = CommandDecoder::default().decode(&mut input).unwrap()
        else {
            panic!("complete literal command expected");
        };
        assert_eq!(&command.body, expected);
        let mut encoded = BytesMut::new();
        CommandEncoder.encode(&command, &mut encoded).unwrap();
        assert_eq!(encoded.as_ref(), *wire);
    }
}

#[test]
fn strict_server_decoder_requests_each_append_literal() {
    let mut decoder = ServerCommandDecoder::default();
    let mut input = BytesMut::from(&b"A4 APPEND {5}\r\n"[..]);
    assert_eq!(
        decoder.decode(&mut input).unwrap(),
        ServerCommandStatus::ContinuationRequired(LiteralRequest { length: 5 })
    );
    decoder.acknowledge_literal().unwrap();
    input.extend_from_slice(b"INBOX {4}\r\n");
    assert_eq!(
        decoder.decode(&mut input).unwrap(),
        ServerCommandStatus::ContinuationRequired(LiteralRequest { length: 4 })
    );
    decoder.acknowledge_literal().unwrap();
    input.extend_from_slice(b"test\r\n");
    let ServerCommandStatus::Complete(command) = decoder.decode(&mut input).unwrap() else {
        panic!("complete APPEND expected");
    };
    assert!(matches!(command.body, CommandBody::Append { .. }));
    assert!(input.is_empty());
}

#[test]
fn command_separators_are_exactly_one_space() {
    for wire in [
        b"A1  NOOP\r\n".as_slice(),
        b"A1\tNOOP\r\n",
        b"A1 NOOP \r\n",
        b"A1 LOGIN user  password\r\n",
        b"A1 LOGIN {4}\r\nuserpassword\r\n",
    ] {
        let mut input = BytesMut::from(wire);
        assert!(CommandDecoder::default().decode(&mut input).is_err());
        assert_eq!(input.as_ref(), wire);
    }
}

#[test]
fn command_decoder_reuses_state_across_complete_independent_frames() {
    let mut decoder = CommandDecoder::default();
    for wire in [
        b"A1 NOOP\r\n".as_slice(),
        b"A2 SELECT INBOX\r\n",
        b"A3 SEARCH ALL\r\n",
        b"A4 CLOSE\r\n",
        b"A5 LOGOUT\r\n",
    ] {
        let mut input = BytesMut::from(wire);
        assert!(matches!(
            decoder.decode(&mut input).unwrap(),
            DecodeStatus::Complete(_)
        ));
        assert!(input.is_empty());
    }
}

#[test]
fn owned_complete_command_parser_is_exact_and_zero_copy() {
    let wire = Bytes::from_static(b"A1 SELECT INBOX\r\n");
    let command = Command::parse(wire.clone()).unwrap();
    assert_eq!(command.tag.as_ptr(), wire.as_ptr());
    assert!(matches!(
        command.body,
        CommandBody::Select { mailbox } if mailbox.as_ref() == b"INBOX"
    ));
    assert!(Command::parse(Bytes::from_static(b"A1 NOOP")).is_err());
    assert!(Command::parse(Bytes::from_static(b"A1 NOOP\r\nA2 NOOP\r\n")).is_err());
}

#[test]
fn borrowed_and_one_backing_commands_cover_every_body_shape() {
    for wire in [
        b"A1 CAPABILITY\r\n".as_slice(),
        b"A1 NOOP\r\n",
        b"A1 LOGOUT\r\n",
        b"A1 STARTTLS\r\n",
        b"A1 IDLE\r\n",
        b"A1 CHECK\r\n",
        b"A1 CLOSE\r\n",
        b"A1 EXPUNGE\r\n",
        b"A1 LOGIN user pass\r\n",
        b"A1 AUTHENTICATE PLAIN =\r\n",
        b"A1 ENABLE IMAP4rev2 UTF8=ACCEPT\r\n",
        b"A1 SELECT INBOX\r\n",
        b"A1 EXAMINE Archive\r\n",
        b"A1 UNSELECT\r\n",
        b"A1 CREATE New\r\n",
        b"A1 DELETE Old\r\n",
        b"A1 RENAME Old New\r\n",
        b"A1 SUBSCRIBE News\r\n",
        b"A1 UNSUBSCRIBE News\r\n",
        b"A1 LIST \"\" *\r\n",
        b"A1 LSUB \"\" *\r\n",
        b"A1 NAMESPACE\r\n",
        b"A1 GETQUOTA \"\"\r\n",
        b"A1 GETQUOTAROOT INBOX\r\n",
        b"A1 SETQUOTA \"\" (STORAGE 100 MESSAGE 20)\r\n",
        b"A1 STATUS INBOX (MESSAGES UIDNEXT)\r\n",
        b"A1 APPEND INBOX {3}\r\nabc\r\n",
        b"A1 ID NIL\r\n",
        b"A1 SEARCH 1:* UNSEEN\r\n",
        b"A1 SORT (REVERSE DATE) UTF-8 ALL\r\n",
        b"A1 THREAD REFERENCES UTF-8 NOT DELETED\r\n",
        b"A1 FETCH 1:4 (UID FLAGS)\r\n",
        b"A1 STORE 1 +FLAGS.SILENT (\\Seen)\r\n",
        b"A1 COPY 1:2 Archive\r\n",
        b"A1 MOVE $ Archive\r\n",
        b"A1 UID FETCH 1 (UID FLAGS)\r\n",
        b"A1 UID SORT (DATE) UTF-8 ALL\r\n",
        b"A1 UID THREAD REFERENCES UTF-8 ALL\r\n",
        b"A1 X-TRACE value\r\n",
        b"A1 X-NOARGS\r\n",
    ] {
        let expected_ref = CommandRef::parse(wire).unwrap_or_else(|error| {
            panic!("valid borrowed command {wire:?} was rejected: {error}")
        });
        let expected_owned = Command::parse(Bytes::copy_from_slice(wire)).unwrap();
        let owned_wire = Bytes::copy_from_slice(wire);
        let pointer = owned_wire.as_ptr();
        let frame = CommandFrame::parse(owned_wire).unwrap();

        assert_eq!(frame.as_bytes().as_ptr(), pointer);
        assert_eq!(frame.as_ref(), expected_ref);
        assert_eq!(frame.into_command(), expected_owned);
    }
}

#[test]
fn sort_thread_and_quota_commands_use_direct_typed_bodies() {
    let cases = [
        b"A1 SORT (REVERSE DATE) UTF-8 ALL\r\n".as_slice(),
        b"A2 THREAD REFERENCES UTF-8 NOT DELETED\r\n",
        b"A3 GETQUOTA \"\"\r\n",
        b"A4 GETQUOTAROOT INBOX\r\n",
        b"A5 SETQUOTA \"\" (STORAGE 100)\r\n",
    ];
    for wire in cases {
        let command = Command::parse(Bytes::copy_from_slice(wire)).unwrap();
        assert!(matches!(
            &command.body,
            CommandBody::Sort { .. }
                | CommandBody::Thread { .. }
                | CommandBody::GetQuota { .. }
                | CommandBody::GetQuotaRoot { .. }
                | CommandBody::SetQuota { .. }
        ));
        let mut encoded = BytesMut::new();
        CommandEncoder.encode(&command, &mut encoded).unwrap();
        assert_eq!(encoded.as_ref(), wire);
    }

    for wire in [
        b"A6 UID SORT (DATE) UTF-8 ALL\r\n".as_slice(),
        b"A7 UID THREAD REFERENCES UTF-8 ALL\r\n",
    ] {
        let command = Command::parse(Bytes::copy_from_slice(wire)).unwrap();
        assert!(matches!(&command.body, CommandBody::Uid { .. }));
        assert!(
            command.parsed_sort_arguments().unwrap().is_some()
                || command.parsed_thread_arguments().unwrap().is_some()
        );
    }
}

#[test]
fn extension_commands_outside_this_batch_remain_raw() {
    for wire in [
        b"A1 COMPRESS DEFLATE\r\n".as_slice(),
        b"A2 GETACL INBOX\r\n",
        b"A3 SETACL INBOX user lr\r\n",
        b"A4 GETMETADATA INBOX /shared/comment\r\n",
    ] {
        let command = Command::parse(Bytes::copy_from_slice(wire)).unwrap();
        assert!(matches!(command.body, CommandBody::Raw { .. }), "{wire:?}");
    }
}

#[test]
fn uid_sort_and_thread_share_strict_validators_and_encode_atomically() {
    for wire in [
        b"A1 UID SORT () UTF-8 ALL\r\n".as_slice(),
        b"A2 UID SORT (DATE) UTF-8 RETURN (COUNT) ALL\r\n",
        b"A3 UID THREAD \"REFERENCES\" UTF-8 ALL\r\n",
        b"A4 UID THREAD REFERENCES UTF-8 CHARSET US-ASCII ALL\r\n",
    ] {
        assert!(
            Command::parse(Bytes::copy_from_slice(wire)).is_err(),
            "{wire:?}"
        );
    }

    let invalid = Command {
        tag: Bytes::from_static(b"A5"),
        body: CommandBody::Uid {
            command: Bytes::from_static(b"SORT"),
            arguments: Bytes::from_static(b"() UTF-8 ALL"),
        },
    };
    let mut destination = BytesMut::from(&b"prefix"[..]);
    assert!(CommandEncoder.encode(&invalid, &mut destination).is_err());
    assert_eq!(destination.as_ref(), b"prefix");
}

#[test]
fn every_complete_command_entry_honors_limits_and_exact_framing() {
    let wire = b"A1 SEARCH (ALL)\r\n";
    let exact = Limits::new(wire.len() - 2, 64, wire.len(), 1);
    assert!(CommandRef::parse_with_limits(wire, exact).is_ok());
    assert!(Command::parse_with_limits(Bytes::from_static(wire), exact).is_ok());
    assert!(CommandFrame::parse_with_limits(Bytes::from_static(wire), exact).is_ok());

    for invalid in [
        b"A1 NOOP".as_slice(),
        b"A1 NOOP\r\nA2 NOOP\r\n",
        b"A1 NOOP\n",
    ] {
        assert!(CommandRef::parse(invalid).is_err());
        assert!(Command::parse(Bytes::copy_from_slice(invalid)).is_err());
        assert!(CommandFrame::parse(Bytes::copy_from_slice(invalid)).is_err());
    }

    let too_small = exact.with_max_frame_len(wire.len() - 1);
    assert_eq!(
        CommandRef::parse_with_limits(wire, too_small)
            .unwrap_err()
            .kind(),
        ErrorKind::FrameTooLarge
    );
    assert_eq!(
        CommandFrame::parse_with_limits(Bytes::from_static(wire), too_small)
            .unwrap_err()
            .kind(),
        ErrorKind::FrameTooLarge
    );

    let too_shallow = exact.with_max_nesting_depth(0);
    assert_eq!(
        CommandRef::parse_with_limits(wire, too_shallow)
            .unwrap_err()
            .kind(),
        ErrorKind::NestingTooDeep
    );
    assert_eq!(
        CommandFrame::parse_with_limits(Bytes::from_static(wire), too_shallow)
            .unwrap_err()
            .kind(),
        ErrorKind::NestingTooDeep
    );
}

#[test]
fn syntax_error_does_not_consume_input() {
    let mut input = BytesMut::from(&b"A1 NOOP extra\r\nnext"[..]);
    let error = CommandDecoder::default().decode(&mut input).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidSyntax);
    assert_eq!(input.as_ref(), b"A1 NOOP extra\r\nnext");
}

#[test]
fn search_nesting_limit_is_configurable_and_atomic() {
    let mut wire = Vec::from(b"A1 SEARCH ".as_slice());
    wire.resize(wire.len() + 65, b'(');
    wire.extend_from_slice(b"ALL");
    wire.resize(wire.len() + 65, b')');
    wire.extend_from_slice(b"\r\n");

    let mut input = BytesMut::from(wire.as_slice());
    let original_pointer = input.as_ptr();
    let error = CommandDecoder::default().decode(&mut input).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NestingTooDeep);
    assert_eq!(input.as_ref(), wire);
    assert_eq!(input.as_ptr(), original_pointer);

    let limits = Limits::default().with_max_nesting_depth(65);
    assert!(matches!(
        CommandDecoder::new(limits).decode(&mut input).unwrap(),
        DecodeStatus::Complete(_)
    ));

    let command = Command {
        tag: Bytes::from_static(b"A1"),
        body: CommandBody::Search {
            criteria: Bytes::copy_from_slice(&wire[b"A1 SEARCH ".len()..wire.len() - 2]),
        },
    };
    let mut output = BytesMut::from(&b"existing"[..]);
    let error = CommandEncoder.encode(&command, &mut output).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NestingTooDeep);
    assert_eq!(output.as_ref(), b"existing");
}

#[test]
fn uid_search_uses_the_same_semantic_validator() {
    let valid = Command::parse(Bytes::from_static(
        b"A1 UID SEARCH RETURN (SAVE) OR UID $ UNSEEN\r\n",
    ))
    .unwrap();
    assert!(valid.parsed_search_program().unwrap().is_some());

    for wire in [
        b"A1 UID SEARCH UID 01\r\n".as_slice(),
        b"A1 UID SEARCH OR ALL\r\n",
    ] {
        assert!(Command::parse(Bytes::copy_from_slice(wire)).is_err());
    }

    let invalid = Command {
        tag: Bytes::from_static(b"A1"),
        body: CommandBody::Uid {
            command: Bytes::from_static(b"SEARCH"),
            arguments: Bytes::from_static(b"UID 01"),
        },
    };
    let mut output = BytesMut::from(&b"existing"[..]);
    assert!(CommandEncoder.encode(&invalid, &mut output).is_err());
    assert_eq!(output.as_ref(), b"existing");
}

#[test]
fn id_command_uses_the_typed_validator_atomically() {
    let valid = Command::parse(Bytes::from_static(
        b"A1 ID (\"name\" \"client\" \"version\" NIL)\r\n",
    ))
    .unwrap();
    let parameters = valid.parsed_id_parameters().unwrap().unwrap();
    assert_eq!(parameters.len(), 2);
    let name = parameters.get(b"NAME").unwrap().decoded().unwrap();
    assert_eq!(name.as_ref(), b"client");

    for wire in [
        b"A1 ID\r\n".as_slice(),
        b"A1 ID name\r\n",
        b"A1 ID (\"name\" \"one\" \"NAME\" \"two\")\r\n",
        b"A1 ID (\"name\" \"client\") trailing\r\n",
    ] {
        let mut input = BytesMut::from(wire);
        let pointer = input.as_ptr();
        assert!(CommandDecoder::default().decode(&mut input).is_err());
        assert_eq!(input.as_ref(), wire);
        assert_eq!(input.as_ptr(), pointer);
    }

    let invalid = Command {
        tag: Bytes::from_static(b"A2"),
        body: CommandBody::Id {
            parameters: Bytes::from_static(b"(\"name\" atom)"),
        },
    };
    let mut output = BytesMut::from(&b"existing"[..]);
    assert!(CommandEncoder.encode(&invalid, &mut output).is_err());
    assert_eq!(output.as_ref(), b"existing");
}

#[test]
fn direct_and_uid_store_copy_move_share_semantic_validation() {
    for (name, arguments, valid) in [
        ("STORE", "1 FLAGS \\Seen", true),
        ("STORE", "2:4 +FLAGS.SILENT (\\Deleted $Junk)", true),
        ("STORE", "$ -FLAGS ()", true),
        ("STORE", "1 X-FLAGS (\\Seen)", false),
        ("STORE", "1 FLAGS (\\Seen  $Junk)", false),
        ("STORE", "1 FLAGS (\\Seen) trailing", false),
        ("STORE", "1  FLAGS \\Seen", false),
        ("STORE", "1 FLAGS  \\Seen", false),
        ("COPY", "1:3 Archive", true),
        ("COPY", "1:3 \"Saved Mail\"", true),
        ("COPY", "1:3 Archive extra", false),
        ("COPY", "0 Archive", false),
        ("MOVE", "4 INBOX", true),
        ("MOVE", "bogus INBOX", false),
        ("MOVE", "4", false),
    ] {
        let direct = Bytes::from(format!("A1 {name} {arguments}\r\n"));
        let uid = Bytes::from(format!("A1 UID {name} {arguments}\r\n"));
        assert_eq!(
            Command::parse(direct).is_ok(),
            valid,
            "direct {name} {arguments}"
        );
        assert_eq!(Command::parse(uid).is_ok(), valid, "UID {name} {arguments}");
    }
}

#[test]
fn recognized_uid_subcommands_are_strict_but_extensions_remain_generic() {
    for wire in [
        b"A1 UID STORE 1 FLAGS \\Seen\r\n".as_slice(),
        b"A2 UID COPY 1:3 \"Saved Mail\"\r\n",
        b"A3 UID MOVE 4 INBOX\r\n",
        b"A4 UID EXPUNGE 7:9\r\n",
        b"A5 UID X-VENDOR opaque arguments\r\n",
    ] {
        let command = Command::parse(Bytes::copy_from_slice(wire)).unwrap();
        assert!(matches!(command.body, CommandBody::Uid { .. }));
        let mut output = BytesMut::new();
        CommandEncoder.encode(&command, &mut output).unwrap();
        assert_eq!(output.as_ref(), wire);
    }

    for wire in [
        b"A1 UID STORE 1 X-FLAGS \\Seen\r\n".as_slice(),
        b"A2 UID STORE 1 FLAGS (\\Seen  custom)\r\n",
        b"A3 UID COPY 1 INBOX extra\r\n",
        b"A4 UID MOVE 1\r\n",
        b"A5 UID EXPUNGE 1 extra\r\n",
        b"A6 UID EXPUNGE 0\r\n",
    ] {
        assert!(
            Command::parse(Bytes::copy_from_slice(wire)).is_err(),
            "accepted {wire:?}"
        );
    }

    let sequence_set = SequenceSet::parse(b"1").unwrap();
    let invalid_store = Command {
        tag: Bytes::from_static(b"A1"),
        body: CommandBody::Store {
            sequence_set,
            operation: StoreOperation::Replace,
            silent: false,
            flags: Bytes::from_static(b"(\\Seen) trailing"),
        },
    };
    let invalid_uid_commands = [
        (b"STORE".as_slice(), b"1 FLAGS (\\Seen  custom)".as_slice()),
        (b"COPY", b"1 INBOX extra"),
        (b"MOVE", b"1"),
        (b"EXPUNGE", b"1 extra"),
    ];

    let mut output = BytesMut::from(&b"existing"[..]);
    assert!(CommandEncoder.encode(&invalid_store, &mut output).is_err());
    assert_eq!(output.as_ref(), b"existing");
    for (command, arguments) in invalid_uid_commands {
        let invalid_uid = Command {
            tag: Bytes::from_static(b"A1"),
            body: CommandBody::Uid {
                command: Bytes::copy_from_slice(command),
                arguments: Bytes::copy_from_slice(arguments),
            },
        };
        assert!(CommandEncoder.encode(&invalid_uid, &mut output).is_err());
        assert_eq!(output.as_ref(), b"existing");
    }
}

#[test]
fn conditional_store_supports_mod_sequence_zero_and_63_bit_boundaries() {
    for wire in [
        b"A1 STORE * (UNCHANGEDSINCE 0) +FLAGS.SILENT (\\Deleted)\r\n".as_slice(),
        b"A2 STORE 1:3 (UNCHANGEDSINCE 9223372036854775807) FLAGS \\Seen\r\n",
        b"A3 UID STORE 4 (UNCHANGEDSINCE 17) -FLAGS ($Processed)\r\n",
    ] {
        let command = Command::parse(Bytes::copy_from_slice(wire)).unwrap();
        if wire.starts_with(b"A3") {
            assert!(matches!(command.body, CommandBody::Uid { .. }));
        } else {
            assert!(matches!(command.body, CommandBody::StoreConditional { .. }));
        }
        let mut encoded = BytesMut::new();
        CommandEncoder.encode(&command, &mut encoded).unwrap();
        assert_eq!(encoded.as_ref(), wire);
    }

    for wire in [
        b"A1 STORE 1 (UNCHANGEDSINCE) FLAGS \\Seen\r\n".as_slice(),
        b"A1 STORE 1 (UNCHANGEDSINCE -1) FLAGS \\Seen\r\n",
        b"A1 STORE 1 (UNCHANGEDSINCE 9223372036854775808) FLAGS \\Seen\r\n",
        b"A1 STORE 1 (UNCHANGEDSINCE 1 UNCHANGEDSINCE 2) FLAGS \\Seen\r\n",
        b"A1 STORE 1 (X-MODIFIER 1) FLAGS \\Seen\r\n",
        b"A1 STORE 1 (UNCHANGEDSINCE 1)  FLAGS \\Seen\r\n",
        b"A1 UID STORE 1 (UNCHANGEDSINCE 1) X-FLAGS \\Seen\r\n",
    ] {
        assert!(
            Command::parse(Bytes::copy_from_slice(wire)).is_err(),
            "accepted {wire:?}"
        );
    }

    let invalid = Command {
        tag: Bytes::from_static(b"A4"),
        body: CommandBody::StoreConditional {
            sequence_set: SequenceSet::parse(b"1").unwrap(),
            unchanged_since: i64::MAX as u64 + 1,
            operation: StoreOperation::Replace,
            silent: false,
            flags: Bytes::from_static(b"\\Seen"),
        },
    };
    let mut output = BytesMut::from(&b"prefix"[..]);
    assert!(CommandEncoder.encode(&invalid, &mut output).is_err());
    assert_eq!(output.as_ref(), b"prefix");
}

#[test]
fn select_and_examine_expose_typed_condstore_qresync_parameters() {
    for wire in [
        b"A1 SELECT INBOX (CONDSTORE)\r\n".as_slice(),
        b"A2 EXAMINE \"Saved Mail\" (QRESYNC (777 12345 1:20 (1:5 10:14)))\r\n",
    ] {
        let command = Command::parse(Bytes::copy_from_slice(wire)).unwrap();
        let arguments = command.parsed_select_arguments().unwrap().unwrap();
        assert!(arguments.parameters().next().is_some());
        assert!(matches!(
            command.body,
            CommandBody::SelectExtended { .. } | CommandBody::ExamineExtended { .. }
        ));

        let mut encoded = BytesMut::new();
        CommandEncoder.encode(&command, &mut encoded).unwrap();
        assert_eq!(encoded.as_ref(), wire);
    }

    for wire in [
        b"A1 SELECT INBOX ()\r\n".as_slice(),
        b"A1 SELECT INBOX (QRESYNC)\r\n",
        b"A1 SELECT INBOX (QRESYNC (0 1))\r\n",
        b"A1 SELECT INBOX (QRESYNC (1 9223372036854775808))\r\n",
        b"A1 SELECT INBOX (CONDSTORE condstore)\r\n",
    ] {
        assert!(
            Command::parse(Bytes::copy_from_slice(wire)).is_err(),
            "accepted {wire:?}"
        );
    }
}

#[test]
fn search_return_extension_depth_uses_decoder_limits_atomically() {
    let mut wire = Vec::from(b"A1 SEARCH RETURN (X-NEST ".as_slice());
    wire.extend(std::iter::repeat_n(b'(', 65));
    wire.push(b'x');
    wire.extend(std::iter::repeat_n(b')', 65));
    wire.extend_from_slice(b") ALL\r\n");

    let mut input = BytesMut::from(wire.as_slice());
    let pointer = input.as_ptr();
    let error = CommandDecoder::default().decode(&mut input).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NestingTooDeep);
    assert_eq!(input.as_ref(), wire);
    assert_eq!(input.as_ptr(), pointer);

    let limits = Limits::default().with_max_nesting_depth(65);
    let DecodeStatus::Complete(command) = CommandDecoder::new(limits).decode(&mut input).unwrap()
    else {
        panic!("complete extended SEARCH expected");
    };
    let mut output = BytesMut::from(&b"existing"[..]);
    let error = CommandEncoder.encode(&command, &mut output).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NestingTooDeep);
    assert_eq!(output.as_ref(), b"existing");
}

#[test]
fn fetch_semantics_are_shared_by_direct_uid_decode_and_encode() {
    for wire in [
        b"A1 FETCH 2:4 (FLAGS BODY[HEADER.FIELDS (DATE FROM)] BINARY.PEEK[2]<0.4096>)\r\n"
            .as_slice(),
        b"A2 UID FETCH 1:* (UID FLAGS MODSEQ) (CHANGEDSINCE 7 VANISHED)\r\n",
        b"A3 FETCH 1 BODY[HEADER.FIELDS ({4+}\r\nDATE)]\r\n",
    ] {
        let command = Command::parse(Bytes::copy_from_slice(wire)).unwrap();
        let parsed = command.parsed_fetch_arguments().unwrap().unwrap();
        assert!(!parsed.items_bytes().is_empty());
        let mut output = BytesMut::new();
        CommandEncoder.encode(&command, &mut output).unwrap();
        assert_eq!(output.as_ref(), wire);
    }

    for wire in [
        b"A1 FETCH 1 (ALL)\r\n".as_slice(),
        b"A2 FETCH 1 BODY[01]\r\n",
        b"A3 UID FETCH 1 FLAGS (VANISHED)\r\n",
        b"A4 FETCH 1 FLAGS (CHANGEDSINCE 7 VANISHED)\r\n",
        b"A5 FETCH 1  FLAGS\r\n",
        b"A6 UID FETCH 1  FLAGS\r\n",
    ] {
        let mut input = BytesMut::from(wire);
        let pointer = input.as_ptr();
        assert!(CommandDecoder::default().decode(&mut input).is_err());
        assert_eq!(input.as_ref(), wire);
        assert_eq!(input.as_ptr(), pointer);
    }

    let invalid = Command {
        tag: Bytes::from_static(b"A5"),
        body: CommandBody::Fetch {
            sequence_set: SequenceSet::parse(b"1").unwrap(),
            items: Bytes::from_static(b"BODY[]<0.0>"),
        },
    };
    let mut output = BytesMut::from(&b"existing"[..]);
    assert!(CommandEncoder.encode(&invalid, &mut output).is_err());
    assert_eq!(output.as_ref(), b"existing");
}

#[test]
fn fetch_modifier_depth_uses_decoder_limits_atomically() {
    let mut wire = Vec::from(b"A1 FETCH 1 FLAGS (X-NEST ".as_slice());
    wire.extend(std::iter::repeat_n(b'(', 65));
    wire.push(b'x');
    wire.extend(std::iter::repeat_n(b')', 65));
    wire.extend_from_slice(b")\r\n");

    let mut input = BytesMut::from(wire.as_slice());
    let pointer = input.as_ptr();
    let error = CommandDecoder::default().decode(&mut input).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NestingTooDeep);
    assert_eq!(input.as_ref(), wire);
    assert_eq!(input.as_ptr(), pointer);

    let limits = Limits::default().with_max_nesting_depth(65);
    assert!(matches!(
        CommandDecoder::new(limits).decode(&mut input).unwrap(),
        DecodeStatus::Complete(_)
    ));
}

#[test]
fn lone_invalid_frame_reuses_its_original_allocation() {
    let mut input = BytesMut::from(&b"A1 NOOP extra\r\n"[..]);
    let original_pointer = input.as_ptr();
    assert!(CommandDecoder::default().decode(&mut input).is_err());
    assert_eq!(input.as_ptr(), original_pointer);
    assert_eq!(input.as_ref(), b"A1 NOOP extra\r\n");
}

#[test]
fn command_encoder_rolls_back_partial_output() {
    let command = Command {
        tag: Bytes::from_static(b"A1"),
        body: CommandBody::Login {
            username: Bytes::from_static(b"user"),
            password: Bytes::from_static(b"bad\npassword"),
        },
    };
    let mut output = BytesMut::from(&b"existing"[..]);
    assert!(CommandEncoder.encode(&command, &mut output).is_err());
    assert_eq!(output.as_ref(), b"existing");
}

#[test]
fn rejects_bare_line_feed_without_consuming() {
    let mut input = BytesMut::from(&b"A1 NOOP\n"[..]);
    let error = CommandDecoder::default().decode(&mut input).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidSyntax);
    assert_eq!(input.as_ref(), b"A1 NOOP\n");
}

#[test]
fn accepts_literal_command_at_every_two_chunk_boundary() {
    let wire = b"A1 XTEST {6}\r\na\r\nbcd\r\n";
    for split in 0..wire.len() {
        let mut decoder = CommandDecoder::default();
        let mut input = BytesMut::from(&wire[..split]);
        assert_eq!(
            decoder.decode(&mut input).unwrap(),
            DecodeStatus::Incomplete,
            "split at byte {split}"
        );
        assert_eq!(input.as_ref(), &wire[..split]);
        input.extend_from_slice(&wire[split..]);
        assert!(matches!(
            decoder.decode(&mut input).unwrap(),
            DecodeStatus::Complete(_)
        ));
        assert!(input.is_empty());
    }
}

#[test]
fn server_decoder_requires_acknowledgement_before_literal_bytes() {
    let mut decoder = ServerCommandDecoder::default();
    let mut input = BytesMut::from(&b"A1 APPEND INBOX {4}\r\n"[..]);
    assert_eq!(
        decoder.decode(&mut input).unwrap(),
        ServerCommandStatus::ContinuationRequired(LiteralRequest { length: 4 })
    );
    assert_eq!(input.as_ref(), b"A1 APPEND INBOX {4}\r\n");

    decoder.acknowledge_literal().unwrap();
    assert_eq!(
        decoder.decode(&mut input).unwrap(),
        ServerCommandStatus::Incomplete
    );
    input.extend_from_slice(b"test\r\n");
    let ServerCommandStatus::Complete(command) = decoder.decode(&mut input).unwrap() else {
        panic!("complete APPEND command expected");
    };
    assert!(matches!(command.body, CommandBody::Append { .. }));
    assert!(input.is_empty());
}

#[test]
fn server_decoder_rejects_premature_synchronizing_literal() {
    let wire = b"A1 APPEND INBOX {4}\r\ntest\r\n";
    let mut input = BytesMut::from(wire.as_slice());
    let error = ServerCommandDecoder::default()
        .decode(&mut input)
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidState);
    assert_eq!(input.as_ref(), wire);
}

#[test]
fn server_decoder_handles_zero_and_multiple_synchronizing_literals() {
    let mut decoder = ServerCommandDecoder::default();
    let mut input = BytesMut::from(&b"A1 XTEST {0}\r\n"[..]);
    assert_eq!(
        decoder.decode(&mut input).unwrap(),
        ServerCommandStatus::ContinuationRequired(LiteralRequest { length: 0 })
    );
    decoder.acknowledge_literal().unwrap();
    input.extend_from_slice(b" {1}\r\n");
    assert_eq!(
        decoder.decode(&mut input).unwrap(),
        ServerCommandStatus::ContinuationRequired(LiteralRequest { length: 1 })
    );
    decoder.acknowledge_literal().unwrap();
    input.extend_from_slice(b"x\r\n");
    assert!(matches!(
        decoder.decode(&mut input).unwrap(),
        ServerCommandStatus::Complete(_)
    ));
}

#[test]
fn client_transmission_waits_before_releasing_literal_bytes() {
    let wire = b"A1 APPEND INBOX {4}\r\ntest\r\n";
    let mut input = BytesMut::from(wire.as_slice());
    let DecodeStatus::Complete(command) = CommandDecoder::default().decode(&mut input).unwrap()
    else {
        panic!("complete APPEND command expected");
    };
    let mut transmission = ClientCommandTransmission::new(&command, false).unwrap();
    assert_eq!(
        transmission.next_step(),
        CommandSendStep::Bytes(Bytes::from_static(b"A1 APPEND INBOX {4}\r\n"))
    );
    let request = CommandSendStep::ContinuationRequired(LiteralRequest { length: 4 });
    assert_eq!(transmission.next_step(), request);
    assert_eq!(transmission.next_step(), request);
    transmission.acknowledge_continuation().unwrap();
    assert_eq!(
        transmission.next_step(),
        CommandSendStep::Bytes(Bytes::from_static(b"test\r\n"))
    );
    assert!(transmission.is_complete());
    assert_eq!(transmission.next_step(), CommandSendStep::Complete);
}

#[test]
fn client_transmission_handles_multiple_and_non_sync_literals() {
    let multi_wire = b"A1 XTEST {1}\r\na {0}\r\n\r\n";
    let mut input = BytesMut::from(multi_wire.as_slice());
    let DecodeStatus::Complete(command) = CommandDecoder::default().decode(&mut input).unwrap()
    else {
        panic!("complete multi-literal command expected");
    };
    let mut transmission = ClientCommandTransmission::new(&command, false).unwrap();
    let mut rebuilt = BytesMut::new();
    loop {
        match transmission.next_step() {
            CommandSendStep::Bytes(bytes) => rebuilt.extend_from_slice(&bytes),
            CommandSendStep::ContinuationRequired(_) => {
                transmission.acknowledge_continuation().unwrap();
            }
            CommandSendStep::Complete => break,
        }
    }
    assert_eq!(rebuilt.as_ref(), multi_wire);

    let non_sync_wire = b"A2 XTEST {5+}\r\nhello\r\n";
    let mut input = BytesMut::from(non_sync_wire.as_slice());
    let DecodeStatus::Complete(command) = CommandDecoder::default().decode(&mut input).unwrap()
    else {
        panic!("complete non-synchronizing command expected");
    };
    let mut transmission = ClientCommandTransmission::new(&command, false).unwrap();
    assert_eq!(
        transmission.next_step(),
        CommandSendStep::Bytes(Bytes::from_static(non_sync_wire))
    );
    assert!(transmission.is_complete());
}

#[test]
fn client_transmission_rejects_invalid_acknowledgement_state() {
    let command = Command {
        tag: Bytes::from_static(b"A1"),
        body: CommandBody::Noop,
    };
    let mut transmission = ClientCommandTransmission::new(&command, false).unwrap();
    assert_eq!(
        transmission.acknowledge_continuation().unwrap_err().kind(),
        ErrorKind::InvalidState
    );
}

#[test]
fn non_synchronizing_literal_obeys_rev2_and_literal_plus_limits() {
    let mut input = BytesMut::from(&b"A1 XTEST {4097+}\r\n"[..]);
    let error = ServerCommandDecoder::default()
        .decode(&mut input)
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::LiteralTooLarge);
    assert_eq!(input.as_ref(), b"A1 XTEST {4097+}\r\n");

    let mut decoder = ServerCommandDecoder::default().with_literal_plus(true);
    assert_eq!(
        decoder.decode(&mut input).unwrap(),
        ServerCommandStatus::Incomplete
    );
}

#[test]
fn response_decoder_accepts_literal8_and_rejects_non_sync_literals() {
    let mut binary = BytesMut::from(&b"* 1 FETCH (BINARY[] ~{4}\r\n\0\r\nx)\r\n"[..]);
    assert!(matches!(
        ResponseDecoder::default().decode(&mut binary).unwrap(),
        DecodeStatus::Complete(Response::Untagged { .. })
    ));
    assert!(binary.is_empty());

    let wire = b"* 1 FETCH (BODY[] {1+}\r\nx)\r\n";
    let mut non_sync = BytesMut::from(wire.as_slice());
    let error = ResponseDecoder::default()
        .decode(&mut non_sync)
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidSyntax);
    assert_eq!(non_sync.as_ref(), wire);
}

#[test]
fn command_decoder_rejects_literal8() {
    let wire = b"A1 XTEST ~{1}\r\n\0\r\n";
    let mut input = BytesMut::from(wire.as_slice());
    let error = CommandDecoder::default().decode(&mut input).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidSyntax);
    assert_eq!(input.as_ref(), wire);
}

#[test]
fn authenticate_continuations_round_trip() {
    let continuations = [
        AuthenticateContinuation::Response(Bytes::new()),
        AuthenticateContinuation::Response(Bytes::from_static(b"dGVzdA==")),
        AuthenticateContinuation::Cancel,
    ];
    for continuation in continuations {
        let mut wire = BytesMut::new();
        AuthenticateContinuationEncoder
            .encode(&continuation, &mut wire)
            .unwrap();
        let DecodeStatus::Complete(decoded) = AuthenticateContinuationDecoder::default()
            .decode(&mut wire)
            .unwrap()
        else {
            panic!("complete AUTHENTICATE continuation expected");
        };
        assert_eq!(decoded, continuation);
        assert!(wire.is_empty());
    }
}

#[test]
fn authenticate_continuation_rejects_invalid_base64_without_consuming() {
    for wire in [
        b"=\r\n".as_slice(),
        b"AAA\r\n",
        b"A=AA\r\n",
        b"AAAA====\r\n",
        b"AA A\r\n",
    ] {
        let mut input = BytesMut::from(wire);
        let error = AuthenticateContinuationDecoder::default()
            .decode(&mut input)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidSyntax);
        assert_eq!(input.as_ref(), wire);
    }
}

#[test]
fn authenticate_continuation_encoder_rolls_back_invalid_base64() {
    let continuation = AuthenticateContinuation::Response(Bytes::from_static(b"A=AA"));
    let mut output = BytesMut::from(&b"existing"[..]);
    assert!(
        AuthenticateContinuationEncoder
            .encode(&continuation, &mut output)
            .is_err()
    );
    assert_eq!(output.as_ref(), b"existing");
}

#[test]
fn authenticate_initial_response_is_validated() {
    for wire in [
        b"A1 AUTHENTICATE PLAIN =\r\n".as_slice(),
        b"A1 AUTHENTICATE PLAIN dGVzdA==\r\n",
    ] {
        let mut input = BytesMut::from(wire);
        assert!(matches!(
            CommandDecoder::default().decode(&mut input).unwrap(),
            DecodeStatus::Complete(_)
        ));
    }

    let invalid = b"A1 AUTHENTICATE PLAIN A=AA\r\n";
    let mut input = BytesMut::from(invalid.as_slice());
    let error = CommandDecoder::default().decode(&mut input).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidSyntax);
    assert_eq!(input.as_ref(), invalid);
}

#[test]
fn idle_done_is_incremental_case_insensitive_and_strict() {
    assert!(CommandRef::parse(b"A1 IDLE extra\r\n").is_err());
    assert!(CommandRef::parse(b"A1 IDLE \r\n").is_err());

    let wire = b"done\r\n";
    for split in 0..wire.len() {
        let mut decoder = IdleDoneDecoder::default();
        let mut input = BytesMut::from(&wire[..split]);
        assert_eq!(
            decoder.decode(&mut input).unwrap(),
            DecodeStatus::Incomplete
        );
        input.extend_from_slice(&wire[split..]);
        assert_eq!(
            decoder.decode(&mut input).unwrap(),
            DecodeStatus::Complete(IdleDone)
        );
        assert!(input.is_empty());
    }

    for invalid in [
        b"DONE extra\r\n".as_slice(),
        b" DONE\r\n",
        b"DONE \r\n",
        b"DONE\n",
    ] {
        let mut input = BytesMut::from(invalid);
        let error = IdleDoneDecoder::default().decode(&mut input).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidSyntax);
        assert_eq!(input.as_ref(), invalid);
    }
}

#[test]
fn extended_typed_commands_round_trip() {
    let commands: &[&[u8]] = &[
        b"A1 AUTHENTICATE SCRAM-SHA-256 aW5pdGlhbA==\r\n",
        b"A2 ENABLE UTF8=ACCEPT CONDSTORE\r\n",
        b"A3 RENAME \"Old Box\" \"New Box\"\r\n",
        b"A4 STATUS INBOX (MESSAGES UIDNEXT)\r\n",
        b"A5 APPEND INBOX {4}\r\ntest\r\n",
        b"A6 FETCH 1:4 (FLAGS BODY.PEEK[])\r\n",
        b"A7 STORE 1,3 +FLAGS.SILENT (\\Seen)\r\n",
        b"A8 MOVE 1:* Archive\r\n",
        b"A9 UID FETCH 5 (FLAGS)\r\n",
        b"A10 LIST \"\" \"*\"\r\n",
        b"A11 ID (\"name\" \"client\")\r\n",
    ];

    for wire in commands {
        let mut input = BytesMut::from(*wire);
        let DecodeStatus::Complete(command) = CommandDecoder::default().decode(&mut input).unwrap()
        else {
            panic!("complete command expected");
        };
        let mut encoded = BytesMut::new();
        CommandEncoder.encode(&command, &mut encoded).unwrap();
        assert_eq!(encoded.as_ref(), *wire);
        assert!(input.is_empty());
    }
}
