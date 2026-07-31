use std::hint::black_box;
use std::time::Duration;

use bytes::BytesMut;
use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use mail_protocol_core::{DecodeStatus, Decoder, Limits};
use mail_protocol_imap::{FetchResponseItem, ResponseDecoder, UntaggedData, parse_untagged};

const NETWORK_CHUNK_BYTES: usize = 16 * 1024;
const BODY_BYTES: usize = 1024 * 1024;

fn metadata_batch() -> Vec<u8> {
    let mut wire = Vec::with_capacity(24 * 1024);
    for index in 0..50_u32 {
        let sequence = index + 1;
        let uid = 80_000 + sequence;
        wire.extend_from_slice(
            format!(
                "* {sequence} FETCH (UID {uid} FLAGS (\\Seen) INTERNALDATE \"21-Jul-2026 12:34:56 +0800\" RFC822.SIZE 2048 ENVELOPE (\"Tue, 21 Jul 2026 12:34:56 +0800\" \"Nivalis protocol benchmark {sequence}\" ((\"Sender\" NIL \"sender\" \"example.test\")) NIL NIL NIL NIL NIL NIL \"<{uid}@example.test>\"))\r\n"
            )
            .as_bytes(),
        );
    }
    wire.extend_from_slice(b"N4 OK FETCH completed\r\n");
    wire
}

fn body_response() -> Vec<u8> {
    let header = format!("* 1 FETCH (UID 80001 BODY[] {{{BODY_BYTES}}}\r\n");
    let mut wire = Vec::with_capacity(header.len() + BODY_BYTES + 3);
    wire.extend_from_slice(header.as_bytes());
    wire.resize(wire.len() + BODY_BYTES, b'x');
    wire.extend_from_slice(b")\r\n");
    wire
}

fn decode_fragmented(wire: &[u8], literal_limit: usize) -> (usize, usize) {
    let limits = Limits::new(64 * 1024, literal_limit, wire.len() + 1024, 256);
    let mut decoder = ResponseDecoder::new(limits);
    let mut input = BytesMut::with_capacity(wire.len());
    let mut frames = 0_usize;
    let mut fetch_bytes = 0_usize;
    for chunk in wire.chunks(NETWORK_CHUNK_BYTES) {
        input.extend_from_slice(chunk);
        while let DecodeStatus::Complete(response) = decoder
            .decode(&mut input)
            .expect("valid benchmark response")
        {
            frames += 1;
            if let Some(UntaggedData::Fetch { data, .. }) =
                parse_untagged(&response).expect("typed benchmark response")
            {
                for item in data.items() {
                    match item {
                        FetchResponseItem::Uid(uid) => {
                            black_box(uid);
                        }
                        FetchResponseItem::Envelope(envelope) => {
                            black_box(envelope.subject().decoded());
                            black_box(envelope.from().iter().next());
                        }
                        FetchResponseItem::BodySection { data, .. } => {
                            fetch_bytes += data.decoded().map_or(0, |value| value.len());
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    assert!(input.is_empty(), "benchmark decoder left a partial frame");
    (frames, fetch_bytes)
}

fn imap_receive_hot_path(criterion: &mut Criterion) {
    let metadata = metadata_batch();
    let mut metadata_group = criterion.benchmark_group("nivalis/imap_receive/metadata");
    metadata_group.sample_size(50);
    metadata_group.measurement_time(Duration::from_secs(3));
    metadata_group.throughput(Throughput::Bytes(metadata.len() as u64));
    metadata_group.bench_function("fragmented_batch_50_decode_and_traverse", |bencher| {
        bencher.iter(|| {
            let result = decode_fragmented(black_box(&metadata), 1024 * 1024);
            assert_eq!(result, (51, 0));
            black_box(result)
        });
    });
    metadata_group.finish();

    let body = body_response();
    let mut body_group = criterion.benchmark_group("nivalis/imap_receive/body");
    body_group.sample_size(20);
    body_group.measurement_time(Duration::from_secs(3));
    body_group.throughput(Throughput::Bytes(body.len() as u64));
    body_group.bench_function("fragmented_literal_1mib_decode_and_traverse", |bencher| {
        bencher.iter_batched_ref(
            || body.clone(),
            |wire| {
                let result = decode_fragmented(black_box(wire), BODY_BYTES);
                assert_eq!(result, (1, BODY_BYTES));
                black_box(result)
            },
            BatchSize::LargeInput,
        );
    });
    body_group.finish();
}

criterion_group!(benches, imap_receive_hot_path);
criterion_main!(benches);
