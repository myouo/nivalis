use bytes::{BufMut, BytesMut};
use mail_protocol_core::{ErrorKind, ProtocolError};

/// One endpoint in an IMAP message sequence range.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Sequence {
    /// A non-zero sequence number or unique identifier.
    Number(u64),
    /// The dynamic `*` endpoint.
    Asterisk,
}

/// One number or inclusive range in an IMAP sequence-set.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SequenceRange {
    /// First endpoint.
    pub start: Sequence,
    /// Second endpoint when this item is a range.
    pub end: Option<Sequence>,
}

/// Parsed IMAP sequence-set.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SequenceSet {
    ranges: Vec<SequenceRange>,
    saved_search: bool,
}

/// Validated, allocation-free view of an IMAP sequence-set.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SequenceSetRef<'a> {
    wire: &'a [u8],
    saved_search: bool,
}

impl<'a> SequenceSetRef<'a> {
    /// Validates and borrows a sequence-set.
    ///
    /// # Errors
    ///
    /// Returns the same syntax and numeric-boundary errors as
    /// [`SequenceSet::parse`].
    pub fn parse(wire: &'a [u8]) -> Result<Self, ProtocolError> {
        if wire == b"$" {
            return Ok(Self {
                wire,
                saved_search: true,
            });
        }
        if wire.is_empty() {
            return Err(invalid_sequence_set());
        }
        validate_ranges(wire)?;
        Ok(Self {
            wire,
            saved_search: false,
        })
    }

    pub(crate) const fn from_validated_ranges(wire: &'a [u8]) -> Self {
        Self::from_validated(wire, false)
    }

    pub(crate) const fn from_validated(wire: &'a [u8], saved_search: bool) -> Self {
        Self { wire, saved_search }
    }

    /// Returns the exact validated wire representation.
    pub const fn as_bytes(self) -> &'a [u8] {
        self.wire
    }

    /// Returns whether this is the saved-search marker `$`.
    pub const fn is_saved_search(self) -> bool {
        self.saved_search
    }

    /// Iterates over parsed ranges without allocating.
    pub const fn ranges(self) -> SequenceRangeIter<'a> {
        SequenceRangeIter {
            inner: RawSequenceRangeIter::new(if self.saved_search { b"" } else { self.wire }),
        }
    }

    /// Creates the existing owning representation.
    pub fn into_owned(self) -> SequenceSet {
        SequenceSet {
            ranges: self.ranges().collect(),
            saved_search: self.saved_search,
        }
    }
}

/// Allocation-free iterator over a validated [`SequenceSetRef`].
#[derive(Clone, Debug)]
pub struct SequenceRangeIter<'a> {
    inner: RawSequenceRangeIter<'a>,
}

impl Iterator for SequenceRangeIter<'_> {
    type Item = SequenceRange;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner
            .next()
            .map(|range| range.expect("SequenceSetRef validates every range"))
    }
}

#[derive(Clone, Debug)]
struct RawSequenceRangeIter<'a> {
    remaining: &'a [u8],
}

impl<'a> RawSequenceRangeIter<'a> {
    const fn new(remaining: &'a [u8]) -> Self {
        Self { remaining }
    }
}

impl Iterator for RawSequenceRangeIter<'_> {
    type Item = Result<SequenceRange, ProtocolError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let comma = self
            .remaining
            .iter()
            .position(|byte| *byte == b',')
            .unwrap_or(self.remaining.len());
        let trailing_comma = comma + 1 == self.remaining.len();
        let range = &self.remaining[..comma];
        self.remaining = if comma == self.remaining.len() {
            b""
        } else {
            &self.remaining[comma + 1..]
        };
        if trailing_comma {
            return Some(Err(invalid_sequence_set()));
        }
        Some(parse_range(range))
    }
}

impl SequenceSet {
    /// Parses a sequence-set containing non-zero 32-bit numbers, `*`, `:`, and `,`.
    ///
    /// # Errors
    ///
    /// Returns an error for empty items, zero, values larger than 32 bits, or any
    /// byte outside the sequence-set grammar.
    pub fn parse(input: &[u8]) -> Result<Self, ProtocolError> {
        if input == b"$" {
            return Ok(Self {
                ranges: Vec::new(),
                saved_search: true,
            });
        }
        if input.is_empty() {
            return Err(invalid_sequence_set());
        }
        Ok(Self {
            ranges: RawSequenceRangeIter::new(input).collect::<Result<_, _>>()?,
            saved_search: false,
        })
    }

    /// Creates a sequence-set from prevalidated ranges.
    ///
    /// # Errors
    ///
    /// Returns an error when `ranges` is empty or contains zero/out-of-range numbers.
    pub fn from_ranges(ranges: Vec<SequenceRange>) -> Result<Self, ProtocolError> {
        if ranges.is_empty()
            || ranges.iter().any(|range| {
                !valid_sequence(range.start) || range.end.is_some_and(|end| !valid_sequence(end))
            })
        {
            Err(invalid_sequence_set())
        } else {
            Ok(Self {
                ranges,
                saved_search: false,
            })
        }
    }

    /// Returns ranges in wire order.
    pub fn ranges(&self) -> &[SequenceRange] {
        &self.ranges
    }

    /// Returns whether this set is the SEARCHRES `$` saved result.
    pub const fn is_saved_search(&self) -> bool {
        self.saved_search
    }

    /// Appends the canonical sequence-set representation.
    pub fn encode(&self, dst: &mut BytesMut) {
        if self.saved_search {
            dst.put_u8(b'$');
            return;
        }
        for (index, range) in self.ranges.iter().enumerate() {
            if index > 0 {
                dst.put_u8(b',');
            }
            encode_sequence(range.start, dst);
            if let Some(end) = range.end {
                dst.put_u8(b':');
                encode_sequence(end, dst);
            }
        }
    }
}

fn parse_range(item: &[u8]) -> Result<SequenceRange, ProtocolError> {
    if item.is_empty() {
        return Err(invalid_sequence_set());
    }
    let mut endpoints = item.split(|byte| *byte == b':');
    let start = parse_sequence(endpoints.next().unwrap_or_default())?;
    let end = endpoints.next().map(parse_sequence).transpose()?;
    if endpoints.next().is_some() {
        return Err(invalid_sequence_set());
    }
    Ok(SequenceRange { start, end })
}

fn validate_ranges(input: &[u8]) -> Result<(), ProtocolError> {
    if input.is_empty() {
        return Err(invalid_sequence_set());
    }
    for item in input.split(|byte| *byte == b',') {
        parse_range(item)?;
    }
    Ok(())
}

/// STORE operation applied to a flag list.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum StoreOperation {
    /// Replace the complete flag set.
    Replace,
    /// Add flags.
    Add,
    /// Remove flags.
    Remove,
}

fn parse_sequence(input: &[u8]) -> Result<Sequence, ProtocolError> {
    if input == b"*" {
        return Ok(Sequence::Asterisk);
    }
    if input.is_empty() || input.first() == Some(&b'0') || !input.iter().all(u8::is_ascii_digit) {
        return Err(invalid_sequence_set());
    }
    let number = input.iter().try_fold(0u64, |number, digit| {
        number
            .checked_mul(10)
            .and_then(|number| number.checked_add(u64::from(*digit - b'0')))
    });
    match number {
        Some(number) if (1..=u64::from(u32::MAX)).contains(&number) => Ok(Sequence::Number(number)),
        _ => Err(invalid_sequence_set()),
    }
}

fn valid_sequence(sequence: Sequence) -> bool {
    match sequence {
        Sequence::Number(number) => (1..=u64::from(u32::MAX)).contains(&number),
        Sequence::Asterisk => true,
    }
}

fn encode_sequence(sequence: Sequence, dst: &mut BytesMut) {
    match sequence {
        Sequence::Number(number) => put_u64(number, dst),
        Sequence::Asterisk => dst.put_u8(b'*'),
    }
}

fn put_u64(value: u64, dst: &mut BytesMut) {
    let mut buffer = [0u8; 20];
    let mut cursor = buffer.len();
    let mut remaining = value;
    loop {
        cursor -= 1;
        buffer[cursor] = b'0' + u8::try_from(remaining % 10).expect("decimal digit fits u8");
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    dst.put_slice(&buffer[cursor..]);
}

fn invalid_sequence_set() -> ProtocolError {
    ProtocolError::new(ErrorKind::InvalidSyntax, "IMAP sequence-set")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_encodes_sequence_set() {
        let sequence_set = SequenceSet::parse(b"1,2:4,*:9").unwrap();
        assert_eq!(sequence_set.ranges().len(), 3);
        let mut encoded = BytesMut::new();
        sequence_set.encode(&mut encoded);
        assert_eq!(encoded.as_ref(), b"1,2:4,*:9");
    }

    #[test]
    fn rejects_zero_empty_and_over_32_bit_values() {
        assert!(SequenceSet::parse(b"0").is_err());
        assert!(SequenceSet::parse(b"01").is_err());
        assert!(SequenceSet::parse(b"1,,2").is_err());
        assert!(SequenceSet::parse(b"1,").is_err());
        assert!(SequenceSet::parse(b"4294967296").is_err());
        assert!(SequenceSetRef::parse(b"1,").is_err());
    }

    #[test]
    fn supports_saved_search_result() {
        let sequence_set = SequenceSet::parse(b"$").unwrap();
        assert!(sequence_set.is_saved_search());
        assert!(sequence_set.ranges().is_empty());
        let mut encoded = BytesMut::new();
        sequence_set.encode(&mut encoded);
        assert_eq!(encoded.as_ref(), b"$");
    }

    #[test]
    fn borrowed_sequence_set_is_zero_copy_and_matches_owned_ranges() {
        let wire = b"1,2:4,*:9";
        let borrowed = SequenceSetRef::parse(wire).unwrap();
        let owned = SequenceSet::parse(wire).unwrap();

        assert_eq!(borrowed.as_bytes().as_ptr(), wire.as_ptr());
        assert_eq!(borrowed.ranges().collect::<Vec<_>>(), owned.ranges());
        assert!(!borrowed.is_saved_search());
    }

    #[test]
    fn borrowed_saved_search_has_no_ranges() {
        let borrowed = SequenceSetRef::parse(b"$").unwrap();
        assert!(borrowed.is_saved_search());
        assert_eq!(borrowed.as_bytes(), b"$");
        assert_eq!(borrowed.ranges().next(), None);
    }
}
