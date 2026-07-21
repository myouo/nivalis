use bytes::{Bytes, BytesMut};
use mail_protocol_core::ErrorKind;

use super::arguments::{MAX_NUMBER64, parse_owned};
use super::*;
use crate::astring::parse_astring_prefix;
use crate::{AStringKind, Command, CommandBody};

#[test]
fn parses_macros_and_every_base_attribute() {
    for (wire, expected) in [
        (b"ALL".as_slice(), FetchMacro::All),
        (b"fast", FetchMacro::Fast),
        (b"Full", FetchMacro::Full),
    ] {
        let parsed = FetchArguments::parse(&Bytes::copy_from_slice(wire)).unwrap();
        assert_eq!(parsed.fetch_macro(), Some(expected));
        assert_eq!(parsed.attributes().count(), 0);
    }

    let wire = Bytes::from_static(
        b"(ENVELOPE FLAGS INTERNALDATE RFC822 RFC822.HEADER RFC822.TEXT RFC822.SIZE BODY BODYSTRUCTURE UID MODSEQ X-GM-MSGID)",
    );
    let parsed = FetchArguments::parse(&wire).unwrap();
    assert_eq!(
        parsed.attributes().collect::<Vec<_>>(),
        vec![
            FetchAttribute::Envelope,
            FetchAttribute::Flags,
            FetchAttribute::InternalDate,
            FetchAttribute::Rfc822,
            FetchAttribute::Rfc822Header,
            FetchAttribute::Rfc822Text,
            FetchAttribute::Rfc822Size,
            FetchAttribute::Body,
            FetchAttribute::BodyStructure,
            FetchAttribute::Uid,
            FetchAttribute::ModSeq,
            FetchAttribute::Other(b"X-GM-MSGID"),
        ]
    );
}

#[test]
fn parses_body_binary_sections_header_lists_and_partials() {
    let wire = Bytes::from_static(
        b"(BODY[] BODY.PEEK[HEADER.FIELDS (DATE \"FROM\" {7+}\r\nSUBJECT)]<0.2048> BODY[3.2.HEADER.FIELDS.NOT (BCC)] BINARY[4.1]<1.9223372036854775807> BINARY.PEEK[] BINARY.SIZE[2])",
    );
    let parsed = FetchArguments::parse(&wire).unwrap();
    let attributes = parsed.attributes().collect::<Vec<_>>();
    assert_eq!(attributes.len(), 6);

    let FetchAttribute::BodySection {
        peek,
        section,
        partial,
    } = attributes[1]
    else {
        panic!("BODY.PEEK expected");
    };
    assert!(peek);
    assert!(section.parts().next().is_none());
    assert_eq!(
        partial,
        Some(FetchPartial {
            offset: 0,
            length: 2048
        })
    );
    let Some(FetchSectionText::HeaderFields(fields)) = section.text() else {
        panic!("HEADER.FIELDS expected");
    };
    assert!(!fields.is_not());
    assert_eq!(
        fields.iter().collect::<Vec<_>>(),
        vec![b"DATE".as_slice(), b"\"FROM\"", b"{7+}\r\nSUBJECT"]
    );

    let FetchAttribute::BodySection { section, .. } = attributes[2] else {
        panic!("BODY section expected");
    };
    assert_eq!(section.parts().collect::<Vec<_>>(), vec![3, 2]);
    let Some(FetchSectionText::HeaderFields(fields)) = section.text() else {
        panic!("HEADER.FIELDS.NOT expected");
    };
    assert!(fields.is_not());

    let FetchAttribute::Binary {
        section, partial, ..
    } = attributes[3]
    else {
        panic!("BINARY expected");
    };
    assert_eq!(section.parts().collect::<Vec<_>>(), vec![4, 1]);
    assert_eq!(
        partial,
        Some(FetchPartial {
            offset: 1,
            length: MAX_NUMBER64,
        })
    );
}

#[test]
fn parses_condstore_qresync_and_generic_modifiers() {
    let wire = Bytes::from_static(
        b"(UID FLAGS MODSEQ) (CHANGEDSINCE 12345 VANISHED X-NEST (one (two three)))",
    );
    let parsed = parse_owned(&wire, 2, Some(true)).unwrap();
    assert_eq!(parsed.extension_depth(), 2);
    assert_eq!(
        parsed.modifiers().collect::<Vec<_>>(),
        vec![
            FetchModifier::ChangedSince(12345),
            FetchModifier::Vanished,
            FetchModifier::Other {
                name: b"X-NEST",
                parameters: Some(b"(one (two three))"),
            },
        ]
    );
    assert_eq!(
        parse_owned(&wire, 1, Some(true)).unwrap_err().kind(),
        ErrorKind::NestingTooDeep
    );
    assert!(parse_owned(&wire, 2, Some(false)).is_err());
    assert!(FetchArguments::parse(&Bytes::from_static(b"FLAGS (VANISHED)")).is_ok());
    assert!(parse_owned(&Bytes::from_static(b"FLAGS (VANISHED)"), 2, Some(true)).is_err());

    let leading_zero =
        FetchArguments::parse(&Bytes::from_static(b"FLAGS (CHANGEDSINCE 01)")).unwrap();
    assert_eq!(
        leading_zero.modifiers().collect::<Vec<_>>(),
        vec![FetchModifier::ChangedSince(1)]
    );
}

#[test]
fn rejects_invalid_fetch_grammar() {
    for wire in [
        b"".as_slice(),
        b"()",
        b"(ALL)",
        b"(FLAGS  UID)",
        b"(FLAGS )",
        b"ALL FLAGS",
        b"BODY[0]",
        b"BODY[01]",
        b"BODY[1HEADER]",
        b"BODY[4294967296]",
        b"BODY[MIME]",
        b"BODY[1.HEADER.FIELDS ()]",
        b"BODY[1.HEADER.FIELDS (DATE  FROM)]",
        b"BODY[]<0.0>",
        b"BODY[]<9223372036854775808.1>",
        b"BINARY[1.TEXT]",
        b"BINARY.SIZE[1]<0.1>",
        b"FLAGS ()",
        b"FLAGS (CHANGEDSINCE 0)",
        b"FLAGS (CHANGEDSINCE 00)",
        b"FLAGS (CHANGEDSINCE 1 CHANGEDSINCE 2)",
    ] {
        assert!(
            FetchArguments::parse(&Bytes::copy_from_slice(wire)).is_err(),
            "{wire:?}"
        );
    }
}

#[test]
fn command_helper_handles_direct_and_uid_fetch() {
    let direct = Command {
        tag: Bytes::from_static(b"A1"),
        body: CommandBody::Fetch {
            sequence_set: crate::SequenceSet::parse(b"1:3").unwrap(),
            items: Bytes::from_static(b"BODY.PEEK[]<0.4096>"),
        },
    };
    assert!(matches!(
        direct
            .parsed_fetch_arguments()
            .unwrap()
            .unwrap()
            .attributes()
            .next(),
        Some(FetchAttribute::BodySection { peek: true, .. })
    ));

    let uid = Command {
        tag: Bytes::from_static(b"A2"),
        body: CommandBody::Uid {
            command: Bytes::from_static(b"FETCH"),
            arguments: Bytes::from_static(b"1:* (UID FLAGS MODSEQ) (CHANGEDSINCE 7 VANISHED)"),
        },
    };
    assert_eq!(
        uid.parsed_fetch_arguments()
            .unwrap()
            .unwrap()
            .modifiers()
            .count(),
        2
    );
}

#[test]
fn header_field_literal_kind_is_length_driven() {
    let parsed =
        FetchArguments::parse(&Bytes::from_static(b"BODY[HEADER.FIELDS ({5+}\r\nA ) B)]")).unwrap();
    let FetchAttribute::BodySection { section, .. } = parsed.attributes().next().unwrap() else {
        panic!("BODY section expected");
    };
    let Some(FetchSectionText::HeaderFields(fields)) = section.text() else {
        panic!("header fields expected");
    };
    assert_eq!(fields.iter().collect::<Vec<_>>(), vec![b"{5+}\r\nA ) B"]);
    let field = fields.iter().next().unwrap();
    assert!(matches!(
        parse_astring_prefix(field).unwrap().kind,
        AStringKind::Literal { .. }
    ));
}

#[test]
fn validated_fetch_encoding_is_exact_and_prefix_preserving() {
    let parsed = FetchArguments::parse(&Bytes::from_static(b"(RFC822 BODY.PEEK[])")).unwrap();
    let mut dst = BytesMut::from(&b"prefix:"[..]);

    parsed.encode(&mut dst).unwrap();

    assert_eq!(&dst[..], b"prefix:(RFC822 BODY.PEEK[])");
}
