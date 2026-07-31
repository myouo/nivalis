use async_imap::imap_proto::{parser::parse_response, types::Response};

const MAX_LITERAL_BYTES: usize = 1024 * 1024;

#[test]
fn patched_parser_accepts_n_and_rejects_n_plus_one_before_reserving() {
    let mut exact = format!("* 1 FETCH (BODY[] {{{MAX_LITERAL_BYTES}}}\r\n").into_bytes();
    exact.resize(exact.len() + MAX_LITERAL_BYTES, b'x');
    exact.extend_from_slice(b")\r\n");
    assert!(matches!(
        parse_response(&exact),
        Ok((_, Response::Fetch(..)))
    ));

    let oversized = format!("* 1 FETCH (BODY[] {{{}}}\r\n", MAX_LITERAL_BYTES + 1);
    let failure = format!("{:?}", parse_response(oversized.as_bytes()).unwrap_err());
    assert!(failure.starts_with("Failure("));
    assert!(failure.contains("TooLarge"));

    assert!(parse_response(b"* OK status text may end in {64}\r\n").is_ok());
}
