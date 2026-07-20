use std::io::{self, BufWriter, Read, Write};

use mail_parser::DateTime;

const MAX_RECIPIENTS: usize = 64;
const MAX_ADDRESS_BYTES: usize = 320;
const MAX_DISPLAY_NAME_BYTES: usize = 320;
const MAX_SUBJECT_BYTES: usize = 998;
const MAX_MESSAGE_ID_LINE_BYTES: usize = 998;
const RECOMMENDED_HEADER_LINE_BYTES: usize = 78;
const HARD_HEADER_LINE_BYTES: usize = 998;
const ENCODED_WORD_PAYLOAD_BYTES: usize = 45;
const STREAM_BUFFER_BYTES: usize = 8 * 1024;
const MIN_DATE_UNIX_SECONDS: i64 = -2_208_988_800;
const MAX_DATE_UNIX_SECONDS: i64 = 32_535_215_999;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OutboundMailbox<'a> {
    address: &'a str,
    display_name: &'a str,
}

impl<'a> OutboundMailbox<'a> {
    pub(crate) fn new(address: &'a str, display_name: &'a str) -> io::Result<Self> {
        validate_address(address)?;
        validate_header_text(display_name, MAX_DISPLAY_NAME_BYTES, true, "display name")?;
        Ok(Self {
            address,
            display_name,
        })
    }
}

#[derive(Debug)]
pub(crate) struct PlainTextMessage<'a> {
    from: OutboundMailbox<'a>,
    to: &'a [OutboundMailbox<'a>],
    subject: &'a str,
    message_id: &'a str,
    date_unix_seconds: i64,
}

impl<'a> PlainTextMessage<'a> {
    pub(crate) fn new(
        from: OutboundMailbox<'a>,
        to: &'a [OutboundMailbox<'a>],
        subject: &'a str,
        message_id: &'a str,
        date_unix_seconds: i64,
    ) -> io::Result<Self> {
        if to.is_empty() || to.len() > MAX_RECIPIENTS {
            return Err(invalid_input(
                "message must have between 1 and 64 To recipients",
            ));
        }
        validate_header_text(subject, MAX_SUBJECT_BYTES, true, "subject")?;
        validate_message_id(message_id)?;
        if !(MIN_DATE_UNIX_SECONDS..=MAX_DATE_UNIX_SECONDS).contains(&date_unix_seconds) {
            return Err(invalid_input("message date is outside the supported range"));
        }
        Ok(Self {
            from,
            to,
            subject,
            message_id,
            date_unix_seconds,
        })
    }

    /// Writes one RFC 5322 message without buffering the body or complete message.
    pub(crate) fn write_to(&self, output: &mut dyn Write, body: &mut dyn Read) -> io::Result<()> {
        let mut output = BufWriter::with_capacity(STREAM_BUFFER_BYTES, output);
        write_ascii_header(
            &mut output,
            "Date",
            DateTime::from_timestamp(self.date_unix_seconds)
                .to_rfc822()
                .as_bytes(),
        )?;
        write_ascii_header(&mut output, "Message-ID", self.message_id.as_bytes())?;
        write_mailbox_header(&mut output, "From", std::slice::from_ref(&self.from))?;
        write_mailbox_header(&mut output, "To", self.to)?;
        write_encoded_header(&mut output, "Subject", self.subject)?;
        output.write_all(b"MIME-Version: 1.0\r\n")?;
        output.write_all(b"Content-Type: text/plain; charset=UTF-8\r\n")?;
        output.write_all(b"Content-Transfer-Encoding: quoted-printable\r\n\r\n")?;
        write_quoted_printable(&mut output, body)?;
        output.flush()
    }
}

fn validate_header_text(
    value: &str,
    maximum: usize,
    allow_empty: bool,
    field: &'static str,
) -> io::Result<()> {
    if (!allow_empty && value.is_empty()) || value.len() > maximum {
        return Err(invalid_input(field));
    }
    if value.bytes().any(|byte| byte < b' ' || byte == 0x7f) {
        return Err(invalid_input(
            "header fields cannot contain control characters",
        ));
    }
    Ok(())
}

fn validate_address(address: &str) -> io::Result<()> {
    validate_header_text(address, MAX_ADDRESS_BYTES, false, "mailbox address")?;
    if !address.is_ascii() {
        return Err(invalid_input("mailbox addresses must be ASCII"));
    }
    let mut parts = address.split('@');
    let local = parts.next().unwrap_or_default();
    let domain = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || local.is_empty()
        || local.len() > 64
        || domain.is_empty()
        || domain.len() > 255
        || local.starts_with('.')
        || local.ends_with('.')
        || local.contains("..")
        || !local.bytes().all(is_dot_atom_byte)
        || !valid_domain(domain)
    {
        return Err(invalid_input(
            "mailbox address is not a supported addr-spec",
        ));
    }
    Ok(())
}

fn is_dot_atom_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'/'
                | b'='
                | b'?'
                | b'^'
                | b'_'
                | b'`'
                | b'{'
                | b'|'
                | b'}'
                | b'~'
                | b'.'
        )
}

fn valid_domain(domain: &str) -> bool {
    domain.len() <= 255
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn validate_message_id(message_id: &str) -> io::Result<()> {
    if message_id.len() + b"Message-ID: ".len() > MAX_MESSAGE_ID_LINE_BYTES
        || !message_id.starts_with('<')
        || !message_id.ends_with('>')
        || !message_id.is_ascii()
    {
        return Err(invalid_input(
            "message id must be a bounded canonical msg-id",
        ));
    }
    let inner = &message_id[1..message_id.len() - 1];
    let mut parts = inner.split('@');
    let left = parts.next().unwrap_or_default();
    let right = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || left.is_empty()
        || right.is_empty()
        || left.starts_with('.')
        || left.ends_with('.')
        || left.contains("..")
        || !left.bytes().all(is_dot_atom_byte)
        || !valid_domain(right)
    {
        return Err(invalid_input(
            "message id is not a supported canonical msg-id",
        ));
    }
    Ok(())
}

fn write_ascii_header(output: &mut dyn Write, name: &str, value: &[u8]) -> io::Result<()> {
    if name.len() + 2 + value.len() > HARD_HEADER_LINE_BYTES {
        return Err(invalid_input("header line exceeds RFC 5322 limit"));
    }
    output.write_all(name.as_bytes())?;
    output.write_all(b": ")?;
    output.write_all(value)?;
    output.write_all(b"\r\n")
}

fn write_encoded_header(output: &mut dyn Write, name: &str, value: &str) -> io::Result<()> {
    output.write_all(name.as_bytes())?;
    output.write_all(b": ")?;
    let mut header = FoldedHeader::new(output, name.len() + 2);
    write_encoded_words(&mut header, value)?;
    header.finish()
}

fn write_mailbox_header(
    output: &mut dyn Write,
    name: &str,
    mailboxes: &[OutboundMailbox<'_>],
) -> io::Result<()> {
    output.write_all(name.as_bytes())?;
    output.write_all(b": ")?;
    let mut header = FoldedHeader::new(output, name.len() + 2);
    for (index, mailbox) in mailboxes.iter().enumerate() {
        if index > 0 {
            header.write_punctuation(b",")?;
            header.fold()?;
        }
        if !mailbox.display_name.is_empty() {
            write_encoded_words(&mut header, mailbox.display_name)?;
        }
        let mut angle_address = [0_u8; MAX_ADDRESS_BYTES + 2];
        angle_address[0] = b'<';
        let end = 1 + mailbox.address.len();
        angle_address[1..end].copy_from_slice(mailbox.address.as_bytes());
        angle_address[end] = b'>';
        header.write_word(&angle_address[..=end])?;
    }
    header.finish()
}

struct FoldedHeader<'a> {
    output: &'a mut dyn Write,
    line_bytes: usize,
    has_word: bool,
}

impl<'a> FoldedHeader<'a> {
    fn new(output: &'a mut dyn Write, line_bytes: usize) -> Self {
        Self {
            output,
            line_bytes,
            has_word: false,
        }
    }

    fn write_word(&mut self, word: &[u8]) -> io::Result<()> {
        let separator = usize::from(self.has_word);
        if self.line_bytes + separator + word.len() > RECOMMENDED_HEADER_LINE_BYTES
            && (self.has_word || self.line_bytes > 1)
        {
            self.fold()?;
        } else if self.has_word {
            self.output.write_all(b" ")?;
            self.line_bytes += 1;
        }
        if self.line_bytes + word.len() > HARD_HEADER_LINE_BYTES {
            return Err(invalid_input("header token exceeds RFC 5322 limit"));
        }
        self.output.write_all(word)?;
        self.line_bytes += word.len();
        self.has_word = true;
        Ok(())
    }

    fn write_punctuation(&mut self, punctuation: &[u8]) -> io::Result<()> {
        if self.line_bytes + punctuation.len() > HARD_HEADER_LINE_BYTES {
            return Err(invalid_input("header line exceeds RFC 5322 limit"));
        }
        self.output.write_all(punctuation)?;
        self.line_bytes += punctuation.len();
        Ok(())
    }

    fn fold(&mut self) -> io::Result<()> {
        self.output.write_all(b"\r\n ")?;
        self.line_bytes = 1;
        self.has_word = false;
        Ok(())
    }

    fn finish(self) -> io::Result<()> {
        self.output.write_all(b"\r\n")
    }
}

fn write_encoded_words(header: &mut FoldedHeader<'_>, value: &str) -> io::Result<()> {
    if value.is_empty() {
        return Ok(());
    }
    let mut word = Vec::with_capacity(12 + ENCODED_WORD_PAYLOAD_BYTES);
    word.extend_from_slice(b"=?UTF-8?Q?");
    let mut payload_bytes = 0;
    for character in value.chars() {
        let encoded_bytes = encoded_character_bytes(character);
        if payload_bytes > 0 && payload_bytes + encoded_bytes > ENCODED_WORD_PAYLOAD_BYTES {
            word.extend_from_slice(b"?=");
            header.write_word(&word)?;
            word.clear();
            word.extend_from_slice(b"=?UTF-8?Q?");
            payload_bytes = 0;
        }
        append_q_encoded_character(&mut word, character);
        payload_bytes += encoded_bytes;
    }
    word.extend_from_slice(b"?=");
    header.write_word(&word)
}

fn encoded_character_bytes(character: char) -> usize {
    let mut utf8 = [0_u8; 4];
    character
        .encode_utf8(&mut utf8)
        .bytes()
        .map(|byte| {
            if byte == b' ' || is_encoded_word_safe(byte) {
                1
            } else {
                3
            }
        })
        .sum()
}

fn append_q_encoded_character(output: &mut Vec<u8>, character: char) {
    let mut utf8 = [0_u8; 4];
    for byte in character.encode_utf8(&mut utf8).bytes() {
        if byte == b' ' {
            output.push(b'_');
        } else if is_encoded_word_safe(byte) {
            output.push(byte);
        } else {
            append_hex_escape(output, byte);
        }
    }
}

fn is_encoded_word_safe(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'!' | b'*' | b'+' | b'-' | b'/')
}

fn write_quoted_printable(output: &mut dyn Write, body: &mut dyn Read) -> io::Result<()> {
    let mut input = [0_u8; STREAM_BUFFER_BYTES];
    let mut line_bytes = 0;
    let mut pending_carriage_return = false;
    let mut ended_with_line_break = true;
    loop {
        let count = body.read(&mut input)?;
        if count == 0 {
            break;
        }
        for &byte in &input[..count] {
            if pending_carriage_return {
                write_body_line_break(output, &mut line_bytes)?;
                ended_with_line_break = true;
                pending_carriage_return = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\r' => pending_carriage_return = true,
                b'\n' => {
                    write_body_line_break(output, &mut line_bytes)?;
                    ended_with_line_break = true;
                }
                _ => {
                    write_body_byte(output, byte, &mut line_bytes)?;
                    ended_with_line_break = false;
                }
            }
        }
    }
    if pending_carriage_return {
        write_body_line_break(output, &mut line_bytes)?;
        ended_with_line_break = true;
    }
    if !ended_with_line_break {
        write_body_line_break(output, &mut line_bytes)?;
    }
    Ok(())
}

fn write_body_byte(output: &mut dyn Write, byte: u8, line_bytes: &mut usize) -> io::Result<()> {
    let encoded_bytes = if is_quoted_printable_literal(byte) {
        1
    } else {
        3
    };
    if *line_bytes + encoded_bytes > 75 {
        output.write_all(b"=\r\n")?;
        *line_bytes = 0;
    }
    if encoded_bytes == 1 {
        output.write_all(&[byte])?;
    } else {
        output.write_all(&hex_escape(byte))?;
    }
    *line_bytes += encoded_bytes;
    Ok(())
}

fn is_quoted_printable_literal(byte: u8) -> bool {
    matches!(byte, b'!'..=b'<' | b'>'..=b'~')
}

fn write_body_line_break(output: &mut dyn Write, line_bytes: &mut usize) -> io::Result<()> {
    output.write_all(b"\r\n")?;
    *line_bytes = 0;
    Ok(())
}

fn append_hex_escape(output: &mut Vec<u8>, byte: u8) {
    output.extend_from_slice(&hex_escape(byte));
}

fn hex_escape(byte: u8) -> [u8; 3] {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    [
        b'=',
        HEX[usize::from(byte >> 4)],
        HEX[usize::from(byte & 0x0f)],
    ]
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_parser::MessageParser;

    const DATE: i64 = 1_720_000_000;

    fn mailbox<'a>(address: &'a str, display_name: &'a str) -> OutboundMailbox<'a> {
        OutboundMailbox::new(address, display_name).expect("valid mailbox")
    }

    fn message<'a>(
        from: OutboundMailbox<'a>,
        to: &'a [OutboundMailbox<'a>],
        subject: &'a str,
    ) -> PlainTextMessage<'a> {
        PlainTextMessage::new(from, to, subject, "<0123456789@nivalis.local>", DATE)
            .expect("valid message")
    }

    #[test]
    fn unicode_headers_and_body_round_trip_without_non_ascii_wire_bytes() {
        let from = mailbox("alice@example.com", "爱丽丝");
        let to = [mailbox("bob@example.net", "Böb")];
        let message = message(from, &to, "部署状态：完成");
        let mut body = "第一行\rsecond\nthird\rfour = \t\n".as_bytes();
        let mut wire = Vec::new();

        message
            .write_to(&mut wire, &mut body)
            .expect("message writes");

        assert!(wire.is_ascii());
        assert!(!wire.windows(5).any(|window| window == b"Bcc: "));
        assert!(wire.windows(9).any(|window| window == b"=?UTF-8?Q"));
        assert!(wire.windows(3).any(|window| window == b"=E7"));
        let parsed = MessageParser::default()
            .parse(&wire)
            .expect("message parses");
        assert_eq!(parsed.subject(), Some("部署状态：完成"));
        assert_eq!(
            parsed
                .from()
                .and_then(|addresses| addresses.first())
                .and_then(|value| value.name()),
            Some("爱丽丝")
        );
        assert_eq!(
            parsed
                .to()
                .and_then(|addresses| addresses.first())
                .and_then(|value| value.name()),
            Some("Böb")
        );
        assert_eq!(
            parsed.body_text(0).as_deref(),
            Some("第一行\r\nsecond\r\nthird\r\nfour = \t\r\n")
        );
    }

    #[test]
    fn rejects_header_injection_and_invalid_or_excessive_fields_before_writing() {
        assert!(OutboundMailbox::new("alice@example.com\r\nBcc:x@example.com", "").is_err());
        assert!(OutboundMailbox::new("alice@example.com", "Alice\nBcc: x@example.com").is_err());
        assert!(OutboundMailbox::new("quoted local@example.com", "").is_err());

        let from = mailbox("alice@example.com", "Alice");
        let to = [mailbox("bob@example.net", "Bob")];
        assert!(
            PlainTextMessage::new(from, &to, "ok\r\nBcc: x@example.com", "<id@host>", DATE)
                .is_err()
        );
        assert!(PlainTextMessage::new(from, &to, "ok", "<id@host>\r\nBcc: x", DATE).is_err());
        assert!(
            PlainTextMessage::new(
                from,
                &to,
                &"s".repeat(MAX_SUBJECT_BYTES + 1),
                "<id@host>",
                DATE
            )
            .is_err()
        );
        assert!(PlainTextMessage::new(from, &[], "ok", "<id@host>", DATE).is_err());
    }

    #[test]
    fn normalizes_every_wire_line_break_and_keeps_lines_bounded() {
        let from = mailbox("alice@example.com", "");
        let to = [mailbox("bob@example.net", "")];
        let subject = "长主题".repeat(80);
        let message = message(from, &to, &subject);
        let body_bytes = format!(
            "{}\n{}\r{}\r\n",
            "a".repeat(300),
            " ".repeat(90),
            "é".repeat(90)
        )
        .into_bytes();
        let mut body = body_bytes.as_slice();
        let mut wire = Vec::new();

        message
            .write_to(&mut wire, &mut body)
            .expect("message writes");

        for (index, byte) in wire.iter().enumerate() {
            if *byte == b'\n' {
                assert!(index > 0 && wire[index - 1] == b'\r');
            }
            if *byte == b'\r' {
                assert_eq!(wire.get(index + 1), Some(&b'\n'));
            }
        }
        let separator = wire
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("header separator");
        assert!(
            wire[..separator]
                .split(|byte| *byte == b'\n')
                .all(|line| line.len() <= HARD_HEADER_LINE_BYTES)
        );
        assert!(
            wire[separator + 4..]
                .split(|byte| *byte == b'\n')
                .all(|line| line.len() <= 77)
        );
    }

    #[test]
    fn stops_streaming_when_the_outer_bound_rejects_output() {
        struct RepeatingBody {
            remaining: usize,
            read: usize,
        }

        impl Read for RepeatingBody {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                let count = self.remaining.min(buffer.len());
                buffer[..count].fill(0xff);
                self.remaining -= count;
                self.read += count;
                Ok(count)
            }
        }

        struct BoundedSink {
            maximum: usize,
            written: usize,
            largest_write: usize,
        }

        impl Write for BoundedSink {
            fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
                self.largest_write = self.largest_write.max(buffer.len());
                if self.written.saturating_add(buffer.len()) > self.maximum {
                    return Err(io::Error::new(io::ErrorKind::FileTooLarge, "test bound"));
                }
                self.written += buffer.len();
                Ok(buffer.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let from = mailbox("alice@example.com", "");
        let to = [mailbox("bob@example.net", "")];
        let message = message(from, &to, "bounded");
        let mut body = RepeatingBody {
            remaining: 10 * 1024 * 1024,
            read: 0,
        };
        let mut sink = BoundedSink {
            maximum: 64 * 1024,
            written: 0,
            largest_write: 0,
        };

        let error = message
            .write_to(&mut sink, &mut body)
            .expect_err("bound fails");

        assert_eq!(error.kind(), io::ErrorKind::FileTooLarge);
        assert!(
            body.read < 128 * 1024,
            "read {} bytes after failure",
            body.read
        );
        assert!(sink.largest_write <= STREAM_BUFFER_BYTES);
    }
}
