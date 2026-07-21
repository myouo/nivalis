use std::borrow::Cow;

use bytes::{Bytes, BytesMut};
use mail_protocol_core::ErrorKind;

use super::*;

#[test]
fn parses_rfc_fetch_response_without_copying() {
    let wire = Bytes::from_static(
        b"(FLAGS (\\Seen $Forwarded) INTERNALDATE \"17-Jul-1996 02:44:25 -0700\" RFC822.SIZE 4286 ENVELOPE (\"Wed, 17 Jul 1996 02:23:25 -0700 (PDT)\" \"summary\" ((\"Terry Gray\" NIL \"gray\" \"cac.washington.edu\")) ((\"Terry Gray\" NIL \"gray\" \"cac.washington.edu\")) ((\"Terry Gray\" NIL \"gray\" \"cac.washington.edu\")) ((NIL NIL \"imap\" \"cac.washington.edu\")) NIL NIL NIL \"<id@example.test>\") BODY (\"TEXT\" \"PLAIN\" (\"CHARSET\" \"US-ASCII\") NIL NIL \"7BIT\" 3028 92) UID 447 MODSEQ (01))",
    );
    let pointer = wire.as_ptr();
    let parsed = FetchResponse::parse(&wire).unwrap();
    assert_eq!(parsed.as_bytes().as_ptr(), pointer);
    assert_eq!(parsed.nesting_depth(), 1);

    let items = parsed.items().collect::<Vec<_>>();
    let FetchResponseItem::Flags(flags) = items[0] else {
        panic!("FLAGS expected");
    };
    assert_eq!(
        flags.iter().collect::<Vec<_>>(),
        vec![FetchFlag::Seen, FetchFlag::Keyword(b"$Forwarded")]
    );
    let FetchResponseItem::Envelope(envelope) = &items[3] else {
        panic!("ENVELOPE expected");
    };
    assert_eq!(envelope.subject().decoded().unwrap().as_ref(), b"summary");
    let from = envelope.from().iter().next().unwrap();
    assert_eq!(from.mailbox.decoded().unwrap().as_ref(), b"gray");
    let FetchResponseItem::Body(body) = items[4] else {
        panic!("BODY expected");
    };
    assert_eq!(body.kind(), BodyStructureKind::Text);
    assert_eq!(body.lines(), Some(92));
    let fields = body.fields().unwrap();
    assert_eq!(fields.octets, 3028);
    let parameter = fields.parameters.iter().next().unwrap();
    assert_eq!(parameter.name.decoded().as_ref(), b"CHARSET");
    assert_eq!(parameter.value.decoded().as_ref(), b"US-ASCII");
    assert!(body.extension_data().is_none());
    assert_eq!(items[5], FetchResponseItem::Uid(447));
    assert_eq!(items[6], FetchResponseItem::ModSeq(1));
}

#[test]
fn accepts_commercial_server_internal_date_with_an_unpadded_day() {
    let wire =
        Bytes::from_static(b"(INTERNALDATE \"2-Jul-2026 08:40:30 +0800\" UID 7 RFC822.SIZE 42)");
    let parsed = FetchResponse::parse(&wire).unwrap();
    let items = parsed.items().collect::<Vec<_>>();
    assert_eq!(
        items[0],
        FetchResponseItem::InternalDate(b"\"2-Jul-2026 08:40:30 +0800\"")
    );
    assert_eq!(items[1], FetchResponseItem::Uid(7));
    assert_eq!(items[2], FetchResponseItem::Rfc822Size(42));
}

#[test]
fn accepts_single_space_between_provider_envelope_addresses() {
    let wire = Bytes::from_static(
        b"(ENVELOPE (NIL NIL ((NIL NIL \"one\" \"example.test\") (NIL NIL \"two\" \"example.test\")) NIL NIL NIL NIL NIL NIL NIL))",
    );
    let parsed = FetchResponse::parse(&wire).unwrap();
    let FetchResponseItem::Envelope(envelope) = parsed.items().next().unwrap() else {
        panic!("ENVELOPE expected");
    };
    let mailboxes = envelope
        .from()
        .iter()
        .map(|address| address.mailbox.decoded().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(mailboxes[0].as_ref(), b"one");
    assert_eq!(mailboxes[1].as_ref(), b"two");

    let malformed = Bytes::from_static(
        b"(ENVELOPE (NIL NIL ((NIL NIL \"one\" \"example.test\")  (NIL NIL \"two\" \"example.test\")) NIL NIL NIL NIL NIL NIL NIL))",
    );
    assert_eq!(
        FetchResponse::parse(&malformed).unwrap_err().kind(),
        ErrorKind::InvalidSyntax
    );
}

#[test]
fn parses_rev1_rfc822_response_items_as_nstrings() {
    let wire = Bytes::from_static(
        b"(RFC822 {4}\r\nbody RFC822.HEADER NIL RFC822.TEXT \"text\" X-RAW atom)",
    );
    let parsed = FetchResponse::parse(&wire).unwrap();
    let items = parsed.items().collect::<Vec<_>>();

    let FetchResponseItem::Rfc822(FetchNString::String(message)) = items[0] else {
        panic!("RFC822 expected");
    };
    assert_eq!(message.decoded().as_ref(), b"body");
    assert_eq!(items[1], FetchResponseItem::Rfc822Header(FetchNString::Nil));
    let FetchResponseItem::Rfc822Text(FetchNString::String(text)) = items[2] else {
        panic!("RFC822.TEXT expected");
    };
    assert_eq!(text.decoded().as_ref(), b"text");
    assert!(matches!(items[3], FetchResponseItem::Other { .. }));
}

#[test]
fn parses_sections_normal_literals_and_literal8() {
    let wire = Bytes::from_static(
        b"(BODY[HEADER]<0> {5}\r\na\r\nbc BINARY[2]<01> ~{4}\r\n\0\r\nx BINARY.SIZE[2] 4)",
    );
    let parsed = FetchResponse::parse(&wire).unwrap();
    let mut items = parsed.items();
    let FetchResponseItem::BodySection {
        section,
        origin,
        data,
    } = items.next().unwrap()
    else {
        panic!("BODY section expected");
    };
    assert_eq!(section.text(), Some(crate::FetchSectionText::Header));
    assert_eq!(origin, Some(0));
    assert_eq!(data.decoded().unwrap().as_ref(), b"a\r\nbc");

    let FetchResponseItem::Binary { origin, data, .. } = items.next().unwrap() else {
        panic!("BINARY expected");
    };
    assert_eq!(origin, Some(1));
    assert!(matches!(
        data,
        FetchBinaryData::Literal8 {
            data: b"\0\r\nx",
            ..
        }
    ));
    assert!(matches!(
        items.next(),
        Some(FetchResponseItem::BinarySize { size: 4, .. })
    ));
}

#[test]
fn body_structure_is_iterative_and_bounded() {
    let mut body = Vec::from(b"(\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 1 1)".as_slice());
    for _ in 1..64 {
        let mut outer = Vec::with_capacity(body.len() + 11);
        outer.push(b'(');
        outer.extend_from_slice(&body);
        outer.extend_from_slice(b" \"MIXED\")");
        body = outer;
    }
    let mut response = Vec::from(b"(BODYSTRUCTURE ".as_slice());
    response.extend_from_slice(&body);
    response.push(b')');
    let response = Bytes::from(response);
    let parsed = FetchResponse::parse(&response).unwrap();
    assert_eq!(parsed.nesting_depth(), 64);

    let mut too_deep = Vec::from(b"(BODYSTRUCTURE (".as_slice());
    too_deep.extend_from_slice(&body);
    too_deep.extend_from_slice(b" \"MIXED\"))");
    assert_eq!(
        FetchResponse::parse(&Bytes::from(too_deep))
            .unwrap_err()
            .kind(),
        ErrorKind::NestingTooDeep
    );
}

#[test]
fn standalone_body_structure_is_zero_copy_bounded_and_encodable() {
    let wire = b"(\"TEXT\" \"PLAIN\" NIL NIL NIL \"8BIT\" 12 2 NIL)";
    let body = BodyStructure::parse(wire).unwrap();
    assert_eq!(body.as_bytes().as_ptr(), wire.as_ptr());
    assert!(body.is_extensible());
    assert_eq!(body.kind(), BodyStructureKind::Text);
    assert_eq!(body.lines(), Some(2));
    assert_eq!(
        BodyStructure::parse_with_max_depth(wire, 0)
            .unwrap_err()
            .kind(),
        ErrorKind::NestingTooDeep
    );
    assert!(BodyStructure::parse(b"(\"TEXT\" \"PLAIN\" NIL NIL NIL \"8BIT\" 1 1) tail").is_err());

    let mut dst = BytesMut::from(&b"prefix:"[..]);
    body.encode(&mut dst).unwrap();
    assert_eq!(&dst[..], &[b"prefix:".as_slice(), wire.as_slice()].concat());
}

#[test]
fn fetch_response_encoding_is_exact_and_prefix_preserving() {
    let response = FetchResponse::parse(&Bytes::from_static(b"(UID 9)")).unwrap();
    let mut dst = BytesMut::from(&b"prefix:"[..]);
    response.encode(&mut dst).unwrap();
    assert_eq!(&dst[..], b"prefix:(UID 9)");
}

#[test]
fn body_numbers_follow_number_and_number64_boundaries() {
    let wire = Bytes::from_static(
        b"(BODYSTRUCTURE (\"TEXT\" \"PLAIN\" NIL NIL NIL \"8BIT\" 4294967295 9223372036854775807))",
    );
    let parsed = FetchResponse::parse(&wire).unwrap();
    let FetchResponseItem::Body(body) = parsed.items().next().unwrap() else {
        panic!("BODYSTRUCTURE expected");
    };
    assert_eq!(body.fields().unwrap().octets, u32::MAX);
    assert_eq!(body.lines(), Some(i64::MAX as u64));
}

#[test]
fn exposes_multipart_and_embedded_message_structure() {
    let multipart = Bytes::from_static(
        b"(BODYSTRUCTURE ((\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"8BIT\" 12 2)(\"APPLICATION\" \"OCTET-STREAM\" NIL NIL NIL \"BASE64\" 16) \"MIXED\" (\"BOUNDARY\" \"x\") NIL (\"en\" \"fr\") \"loc\" (NIL 1 (\"future\"))))",
    );
    let parsed = FetchResponse::parse(&multipart).unwrap();
    let FetchResponseItem::Body(body) = parsed.items().next().unwrap() else {
        panic!("multipart BODYSTRUCTURE expected");
    };
    assert_eq!(body.kind(), BodyStructureKind::Multipart);
    assert_eq!(body.subtype().decoded().as_ref(), b"MIXED");
    assert!(body.extension_data().is_some());
    let extensions = body.extensions().unwrap();
    let boundary = extensions.parameters().unwrap().iter().next().unwrap();
    assert_eq!(boundary.name.decoded().as_ref(), b"BOUNDARY");
    assert_eq!(boundary.value.decoded().as_ref(), b"x");
    assert_eq!(extensions.disposition(), Some(BodyDisposition::Nil));
    assert_eq!(
        extensions
            .language()
            .unwrap()
            .iter()
            .map(FetchString::decoded)
            .collect::<Vec<_>>(),
        vec![
            Cow::Borrowed(b"en".as_slice()),
            Cow::Borrowed(b"fr".as_slice())
        ]
    );
    assert_eq!(
        extensions.location().unwrap().decoded().unwrap().as_ref(),
        b"loc"
    );
    assert_eq!(
        extensions.future().collect::<Vec<_>>(),
        vec![b"(NIL 1 (\"future\"))".as_slice()]
    );
    let parts = body.parts().collect::<Vec<_>>();
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].kind(), BodyStructureKind::Text);
    assert_eq!(parts[0].lines(), Some(2));
    assert_eq!(parts[1].kind(), BodyStructureKind::Basic);

    let message = Bytes::from_static(
        b"(BODYSTRUCTURE (\"MESSAGE\" \"RFC822\" NIL NIL NIL \"7BIT\" 42 (NIL NIL NIL NIL NIL NIL NIL NIL NIL NIL) (\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 7 1) 3 NIL))",
    );
    let parsed = FetchResponse::parse(&message).unwrap();
    let FetchResponseItem::Body(body) = parsed.items().next().unwrap() else {
        panic!("message BODYSTRUCTURE expected");
    };
    assert_eq!(body.kind(), BodyStructureKind::Message);
    assert_eq!(body.lines(), Some(3));
    assert!(body.envelope().is_some());
    let embedded = body.embedded_body().unwrap();
    assert_eq!(embedded.kind(), BodyStructureKind::Text);
    assert_eq!(embedded.lines(), Some(1));
    assert_eq!(body.extension_data(), Some(b"NIL".as_slice()));
    assert_eq!(body.extensions().unwrap().md5(), Some(FetchNString::Nil));
}

#[test]
fn rejects_invalid_fetch_response_grammar() {
    for wire in [
        b"()".as_slice(),
        b"(FLAGS  (\\Seen))",
        b"(FLAGS (\\))",
        b"(UID 0)",
        b"(UID 01)",
        b"(MODSEQ (0))",
        b"(RFC822.SIZE 9223372036854775808)",
        b"(RFC822 atom)",
        b"(BODY[01] NIL)",
        b"(BINARY[1.TEXT] NIL)",
        b"(BODY (\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 1 1 NIL))",
        b"(BODYSTRUCTURE (\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 1))",
        b"(BODYSTRUCTURE (\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 4294967296 1))",
        b"(BODYSTRUCTURE (\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 1 9223372036854775808))",
        b"(ENVELOPE (NIL NIL () NIL NIL NIL NIL NIL NIL NIL))",
    ] {
        assert!(
            FetchResponse::parse(&Bytes::copy_from_slice(wire)).is_err(),
            "{wire:?}"
        );
    }
}
