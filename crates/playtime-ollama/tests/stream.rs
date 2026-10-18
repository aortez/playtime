use playtime_ollama::*;
use serde_json::json;

fn decode(
    bytes: &[u8],
    limits: Limits,
) -> (
    Decoder,
    playtime_ollama::Result<playtime_engine::ModelResponse>,
    Vec<Observation>,
) {
    let (tx, mut rx) = observations(&limits).unwrap();
    let mut decoder = Decoder::new(limits).unwrap();
    let result = decoder
        .feed(bytes, &tx)
        .or_else(|| decoder.eof(&tx))
        .unwrap();
    let mut records = vec![];
    while let Some(record) = rx.try_recv() {
        records.push(record);
    }
    (decoder, result, records)
}
#[test]
fn every_byte_boundary_crlf_unicode_tools_and_final_frame_are_preserved() {
    let bytes = include_bytes!("fixtures/stream.ndjson");
    let expected: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/stream.expected.json")).unwrap();
    // Every possible two-chunk split, plus single bytes, multiple frames/read.
    for split in 0..=bytes.len() {
        let (tx, mut rx) = observations(&Limits::default()).unwrap();
        let mut decoder = Decoder::new(Limits::default()).unwrap();
        assert!(decoder.feed(&bytes[..split], &tx).is_none());
        assert!(decoder.feed(&bytes[split..], &tx).is_none());
        let response = decoder.eof(&tx).unwrap().unwrap();
        assert_eq!(response.message, expected);
        assert_eq!(response.generated_tokens, Some(32));
        assert_eq!(response.prompt_tokens, Some(48));
        let mut raw = vec![];
        while let Some(record) = rx.try_recv() {
            assert_eq!(record.kind, ObservationKind::NativeChunk);
            raw.extend(record.bytes);
        }
        assert_eq!(raw, bytes);
        assert!(decoder.feed(b"{}\n", &tx).is_none());
        assert!(decoder.eof(&tx).is_none());
    }
    let (tx, mut rx) = observations(&Limits::default()).unwrap();
    let mut decoder = Decoder::new(Limits::default()).unwrap();
    for b in bytes {
        assert!(decoder.feed(&[*b], &tx).is_none());
    }
    assert_eq!(decoder.eof(&tx).unwrap().unwrap().message, expected);
    assert_eq!((0..3).filter_map(|_| rx.try_recv()).count(), 3);
}
#[test]
fn first_terminal_ends_without_eof_and_discards_duplicates() {
    let wire =
        b"{\"message\":{\"content\":\"\",\"tool_calls\":[]},\"done\":true,\"eval_count\":0}\n";
    let mut combined = wire.to_vec();
    combined.extend_from_slice(b"garbage\n{\"done\":true,\"eval_count\":100}\n");
    let (decoder, result, records) = decode(&combined, Limits::default());
    let response = result.unwrap();
    assert_eq!(response.generated_tokens, Some(0));
    assert_eq!(
        response.message,
        json!({"role":"assistant","content":"","tool_calls":[]})
    );
    assert_eq!(decoder.records(), 1);
    assert_eq!(decoder.consumed_bytes(), wire.len());
    assert_eq!(records.len(), 1);
    let (_, response, _) = decode(
        b"{\"done\":true,\"eval_count\":999999}\n",
        Limits::default(),
    );
    assert_eq!(response.unwrap().generated_tokens, Some(999999));
}
#[test]
fn invalid_usage_never_becomes_known_zero_or_dispatchable_tools() {
    for usage in ["null", "-1", "1.0", "1.5", "true", "18446744073709551616"] {
        let body = format!(
            "{{\"message\":{{\"content\":\"retained\"}},\"done\":true,\"eval_count\":{usage}}}\n"
        );
        let (d, result, records) = decode(body.as_bytes(), Limits::default());
        assert_eq!(
            result.unwrap_err().kind,
            FailureKind::MissingUsage,
            "{usage}"
        );
        assert!(d.terminal_observed());
        assert_eq!(d.partial().content.as_deref(), Some("retained"));
        assert_eq!(records.len(), 1);
    }
    assert_eq!(
        decode(b"{\"done\":true}\n", Limits::default())
            .1
            .unwrap_err()
            .kind,
        FailureKind::MissingUsage
    );
    for (usage, expected) in [("-0", 0), ("18446744073709551615", u64::MAX)] {
        assert_eq!(
            decode(
                format!("{{\"done\":true,\"eval_count\":{usage}}}").as_bytes(),
                Limits::default()
            )
            .1
            .unwrap()
            .generated_tokens,
            Some(expected)
        );
    }
}
#[test]
fn malformed_protocol_is_distinct_from_legacy_backend_parse_error() {
    for bytes in [
        b"\n".as_slice(),
        b" \r\n",
        b"{",
        b"{\"done\":true,\"done\":false}\n",
        b"[]\n",
        b"{\"done\":1}\n",
        b"{\"message\":null}\n",
        b"{\"message\":{\"role\":\"user\"}}\n",
        b"{\"message\":{\"content\":3}}\n",
        b"{\"message\":{\"thinking\":false}}\n",
        b"{\"message\":{\"tool_calls\":[{\"function\":{\"name\":\"x\",\"arguments\":\"{}\"}}]}}\n",
        b"{\"error\":false}\n",
        b"\xff\n",
    ] {
        let (d, result, records) = decode(bytes, Limits::default());
        assert_eq!(result.unwrap_err().kind, FailureKind::Protocol, "{bytes:?}");
        assert!(!d.terminal_observed());
        assert_eq!(records[0].bytes, bytes);
    }
    for (error, kind) in [
        (
            "error parsing tool call: synthetic",
            FailureKind::BackendToolParse,
        ),
        ("Error parsing tool call: x", FailureKind::Http),
        ("error parsing tool call", FailureKind::Http),
        ("prefix error parsing tool call: x", FailureKind::Http),
    ] {
        let body = serde_json::to_vec(
            &json!({"error":error,"done":true,"eval_count":0,"message":{"content":"ignored"}}),
        )
        .unwrap();
        let (d, result, _) = decode(&body, Limits::default());
        assert_eq!(result.unwrap_err().kind, kind);
        assert_eq!(d.partial().content, None);
        assert!(!d.terminal_observed());
    }
}
#[test]
fn incomplete_and_limit_edges_preserve_bounded_partial_evidence() {
    let prefix = b"{\"message\":{\"content\":\"partial\"},\"done\":false}\n";
    let (d, result, records) = decode(prefix, Limits::default());
    assert_eq!(result.unwrap_err().kind, FailureKind::Incomplete);
    assert_eq!(d.partial().content.as_deref(), Some("partial"));
    assert_eq!(records.len(), 1);
    let done = b"{\"done\":true,\"eval_count\":0}\n";
    assert!(
        decode(
            done,
            Limits {
                record_bytes: done.len(),
                response_bytes: done.len(),
                ..Default::default()
            }
        )
        .1
        .is_ok()
    );
    for limits in [
        Limits {
            record_bytes: done.len() - 1,
            ..Default::default()
        },
        Limits {
            response_bytes: done.len() - 1,
            ..Default::default()
        },
        Limits {
            queue_bytes: done.len() - 1,
            ..Default::default()
        },
    ] {
        let (d, r, _) = decode(done, limits);
        assert_eq!(r.unwrap_err().kind, FailureKind::ByteLimit);
        assert!(!d.pending_fragment().is_empty());
    }
    let mut body = prefix.to_vec();
    body.extend_from_slice(done);
    for limits in [
        Limits {
            records: 1,
            ..Default::default()
        },
        Limits {
            queue_records: 1,
            ..Default::default()
        },
        Limits {
            assembled_bytes: 6,
            ..Default::default()
        },
    ] {
        assert_eq!(
            decode(&body, limits).1.unwrap_err().kind,
            FailureKind::ByteLimit
        );
    }
    let (tx, mut rx) = observations(&Limits {
        queue_records: 1,
        queue_bytes: prefix.len(),
        ..Default::default()
    })
    .unwrap();
    let mut d = Decoder::new(Limits::default()).unwrap();
    assert!(d.feed(prefix, &tx).is_none());
    assert!(rx.try_recv().is_some());
    assert!(d.feed(done, &tx).unwrap().is_ok());
    let deep = format!("{{\"extra\":{}0{}}}\n", "[".repeat(64), "]".repeat(64));
    assert_eq!(
        decode(deep.as_bytes(), Limits::default())
            .1
            .unwrap_err()
            .kind,
        FailureKind::Protocol
    );
}
