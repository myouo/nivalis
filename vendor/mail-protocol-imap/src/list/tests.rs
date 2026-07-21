use bytes::{Bytes, BytesMut};
use mail_protocol_core::{Decoder, Encoder, ErrorKind, Limits};

use super::options::selection_option_prefilter_bit;
use super::*;
use crate::{
    Command, CommandBody, CommandDecoder, CommandEncoder, Response, ResponseEncoder, Status,
    UntaggedData, parse_untagged,
};

#[test]
fn parses_basic_and_extended_rfc_list_commands() {
    let basic = ListArguments::parse(&Bytes::from_static(b"\"\" ~/Mail/%")).unwrap();
    assert_eq!(basic.reference().decoded().as_ref(), b"");
    assert_eq!(basic.pattern().decoded().as_ref(), b"~/Mail/%");
    assert_eq!(basic.patterns().count(), 1);
    assert!(!basic.has_parenthesized_pattern());

    let extended = ListArguments::parse(&Bytes::from_static(
        b"(SUBSCRIBED REMOTE RECURSIVEMATCH) \"\" (foo/*) RETURN (CHILDREN STATUS (MESSAGES UNSEEN) X-OPT (one (two)))",
    ))
    .unwrap();
    assert!(extended.has_parenthesized_pattern());
    assert_eq!(extended.pattern().decoded().as_ref(), b"foo/*");
    assert_eq!(extended.extension_depth(), 2);
    assert_eq!(
        extended.selection_option_items().collect::<Vec<_>>(),
        vec![
            ListSelectionOption::Subscribed,
            ListSelectionOption::Remote,
            ListSelectionOption::RecursiveMatch,
        ]
    );
    assert_eq!(
        extended.return_option_items().collect::<Vec<_>>(),
        vec![
            ListReturnOption::Children,
            ListReturnOption::Status {
                items: b"(MESSAGES UNSEEN)"
            },
            ListReturnOption::Other {
                name: b"X-OPT",
                parameters: Some(b"(one (two))")
            },
        ]
    );
}

#[test]
fn parses_rfc5258_multiple_patterns_without_payload_copies() {
    let wire = Bytes::from_static(b"\"\" (foo/% \"bar/*\" {5+}\r\nbaz/%)");
    let arguments = ListArguments::parse(&wire).unwrap();
    assert!(arguments.has_parenthesized_pattern());
    assert_eq!(arguments.pattern().decoded().as_ref(), b"foo/%");

    let patterns = arguments.patterns().collect::<Vec<_>>();
    assert_eq!(patterns.len(), 3);
    assert_eq!(patterns[0].decoded().as_ref(), b"foo/%");
    assert_eq!(patterns[1].decoded().as_ref(), b"bar/*");
    assert_eq!(patterns[2].decoded().as_ref(), b"baz/%");
    for pattern in &patterns {
        let pointer = pattern.as_bytes().as_ptr();
        assert!(pointer >= wire.as_ptr());
        assert!(pointer < wire.as_ptr_range().end);
    }

    let command = Command::parse(Bytes::from_static(b"A1 LIST \"\" (one two)\r\n")).unwrap();
    let command_arguments = command.parsed_list_arguments().unwrap().unwrap();
    let mut command_patterns = command_arguments.patterns();
    assert_eq!(command_patterns.next().unwrap().decoded().as_ref(), b"one");
    assert_eq!(command_patterns.next().unwrap().decoded().as_ref(), b"two");
    assert!(command_patterns.next().is_none());
}

#[test]
fn enforces_list_command_semantics_and_nesting() {
    for invalid_wire in [
        b"".as_slice(),
        b"\"\"",
        b"(RECURSIVEMATCH) \"\" *",
        b"(REMOTE RECURSIVEMATCH) \"\" *",
        b"()  \"\" *",
        b"\"\" ()",
        b"\"\" (one  two)",
        b"\"\" (one )",
        b"\"\" * RETURN (STATUS ())",
        b"\"\" * RETURN (CHILDREN )",
    ] {
        assert!(
            ListArguments::parse(&Bytes::copy_from_slice(invalid_wire)).is_err(),
            "{invalid_wire:?}"
        );
    }
    let nested = Bytes::from_static(b"(X (one (two))) \"\" *");
    assert_eq!(
        ListArguments::parse_with_max_depth(&nested, 1)
            .unwrap_err()
            .kind(),
        ErrorKind::NestingTooDeep
    );
    assert!(ListArguments::parse_with_max_depth(&nested, 2).is_ok());
}

#[test]
fn validated_list_encoding_is_exact_and_prefix_preserving() {
    let arguments = ListArguments::parse(&Bytes::from_static(b"\"\" (one two)")).unwrap();
    let response =
        ListResponse::parse(&Bytes::from_static(b"LIST (\\HasChildren) \"/\" one")).unwrap();
    let mut dst = BytesMut::from(&b"prefix:"[..]);
    arguments.encode(&mut dst).unwrap();
    assert_eq!(&dst[..], b"prefix:\"\" (one two)");

    dst.truncate(b"prefix:".len());
    response.encode(&mut dst).unwrap();
    assert_eq!(&dst[..], b"prefix:LIST (\\HasChildren) \"/\" one");
}

#[test]
fn parses_rfc_list_responses_and_extended_items() {
    let response = ListResponse::parse(&Bytes::from_static(
        b"LIST (\\Subscribed \\HasChildren) \"/\" \"Foo\" (\"CHILDINFO\" (\"SUBSCRIBED\") OLDNAME (\"OldFoo\") X-COUNT 3)",
    ))
    .unwrap();
    assert_eq!(response.delimiter(), Some(b'/'));
    assert_eq!(response.mailbox().decoded().as_ref(), b"Foo");
    assert!(response.exists());
    assert!(response.is_selectable());
    assert_eq!(response.has_children(), Some(true));
    assert_eq!(
        response.attributes().collect::<Vec<_>>(),
        vec![ListAttribute::Subscribed, ListAttribute::HasChildren]
    );
    assert_eq!(
        response.extended_items().collect::<Vec<_>>(),
        vec![
            ListExtendedItem::ChildInfo {
                options: b"(\"SUBSCRIBED\")"
            },
            ListExtendedItem::OldName {
                mailbox: b"\"OldFoo\""
            },
            ListExtendedItem::Other {
                name: b"X-COUNT",
                value: b"3"
            },
        ]
    );

    let missing = ListResponse::parse(&Bytes::from_static(
        b"LIST (\\NonExistent \\Noinferiors) NIL old",
    ))
    .unwrap();
    assert!(!missing.exists());
    assert!(!missing.is_selectable());
    assert_eq!(missing.has_children(), Some(false));
}

#[test]
fn matches_childinfo_base_options_without_allocating_candidate_sets() {
    let request = ListArguments::parse(&Bytes::from_static(
        b"(X-OPT (\"value\") RECURSIVEMATCH) \"\" *",
    ))
    .unwrap();
    assert!(request.has_recursive_match());

    let matching = ListResponse::parse(&Bytes::from_static(
        b"LIST () \"/\" parent (CHILDINFO (\"x-opt (\\\"value\\\")\"))",
    ))
    .unwrap();
    assert!(matching.has_child_info());
    assert!(request.correlates_child_info(&matching));

    let mismatched = ListResponse::parse(&Bytes::from_static(
        b"LIST () \"/\" parent (CHILDINFO (\"X-OPT (other)\"))",
    ))
    .unwrap();
    assert!(!request.correlates_child_info(&mismatched));
}

#[test]
fn prefilter_collisions_still_require_exact_childinfo_match() {
    let mut by_bit: [Option<Vec<u8>>; 64] = std::array::from_fn(|_| None);
    let (left, right) = (0u32..256)
        .find_map(|index| {
            let name = format!("X-{index}").into_bytes();
            let bit = selection_option_prefilter_bit(ListSelectionOption::Other {
                name: &name,
                parameters: None,
            })
            .trailing_zeros() as usize;
            if let Some(previous) = by_bit[bit].as_ref() {
                return Some((previous.clone(), name));
            }
            by_bit[bit] = Some(name);
            None
        })
        .expect("pigeonhole principle guarantees a 64-bit prefilter collision");

    let request = ListArguments::parse(&Bytes::from(format!(
        "({} RECURSIVEMATCH) \"\" *",
        String::from_utf8(left).unwrap()
    )))
    .unwrap();
    let response = ListResponse::parse(&Bytes::from(format!(
        "LIST () \"/\" parent (CHILDINFO (\"{}\"))",
        String::from_utf8(right).unwrap()
    )))
    .unwrap();
    assert_eq!(
        request.child_info_prefilter(),
        response.child_info_prefilter().unwrap()
    );
    assert!(!request.correlates_child_info(&response));
}

#[test]
fn rejects_invalid_list_response_attributes_and_values() {
    for invalid_wire in [
        b"LIST (\\Marked \\Unmarked) \"/\" INBOX".as_slice(),
        b"LIST (\\HasChildren \\HasNoChildren) \"/\" INBOX",
        b"LIST (\\Remote \\remote) \"/\" INBOX",
        b"LIST () \"//\" INBOX",
        b"LIST () \"/\" {5+}\r\nINBOX",
        b"LIST () \"/\" INBOX (OLDNAME ())",
        b"LIST () \"/\" INBOX (CHILDINFO (SUBSCRIBED))",
    ] {
        assert!(
            ListResponse::parse(&Bytes::copy_from_slice(invalid_wire)).is_err(),
            "{invalid_wire:?}"
        );
    }
}

#[test]
fn command_codec_shares_list_validation_and_limits_atomically() {
    let wire = Bytes::from_static(
        b"A1 LIST (SUBSCRIBED RECURSIVEMATCH) \"\" * RETURN (STATUS (MESSAGES))\r\n",
    );
    let command = Command::parse(wire.clone()).unwrap();
    let arguments = command.parsed_list_arguments().unwrap().unwrap();
    assert_eq!(arguments.pattern().decoded().as_ref(), b"*");

    let mut encoded = BytesMut::new();
    CommandEncoder.encode(&command, &mut encoded).unwrap();
    assert_eq!(encoded.as_ref(), wire.as_ref());

    let nested = b"A1 LIST (X (one (two))) \"\" *\r\n";
    let limits = Limits::default().with_max_nesting_depth(1);
    assert_eq!(
        Command::parse_with_limits(Bytes::from_static(nested), limits)
            .unwrap_err()
            .kind(),
        ErrorKind::NestingTooDeep
    );
    let mut input = BytesMut::from(nested.as_slice());
    let original = input.clone();
    let error = CommandDecoder::new(limits).decode(&mut input).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NestingTooDeep);
    assert_eq!(input, original);

    let invalid = Command {
        tag: Bytes::from_static(b"A2"),
        body: CommandBody::List {
            arguments: Bytes::from_static(b"(RECURSIVEMATCH) \"\" *"),
        },
    };
    let mut output = BytesMut::from(&b"prefix"[..]);
    let original = output.clone();
    assert!(CommandEncoder.encode(&invalid, &mut output).is_err());
    assert_eq!(output, original);
}

#[test]
fn response_dispatch_and_encoder_share_typed_list_validation() {
    let wire = Bytes::from_static(b"* LIST () \"/\" {3}\r\nfoo\r\n");
    let response = Response::parse(wire.clone()).unwrap();
    let Some(UntaggedData::List(list)) = parse_untagged(&response).unwrap() else {
        panic!("LIST response must use the typed dispatch path");
    };
    assert_eq!(list.mailbox().decoded().as_ref(), b"foo");

    let mut encoded = BytesMut::new();
    ResponseEncoder.encode(&response, &mut encoded).unwrap();
    assert_eq!(encoded.as_ref(), wire.as_ref());

    let invalid = Response::Untagged {
        data: Bytes::from_static(b"LIST (\\Marked \\Unmarked) \"/\" INBOX"),
    };
    let mut output = BytesMut::from(&b"prefix"[..]);
    let original = output.clone();
    assert!(ResponseEncoder.encode(&invalid, &mut output).is_err());
    assert_eq!(output, original);

    let completion = Response::Tagged {
        tag: Bytes::from_static(b"A1"),
        status: Status::Ok,
        information: Bytes::from_static(b"LIST completed"),
    };
    let mut output = BytesMut::new();
    ResponseEncoder.encode(&completion, &mut output).unwrap();
    assert_eq!(output.as_ref(), b"A1 OK LIST completed\r\n");
}
