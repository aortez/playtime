use playtime_docker::{capture::*, *};
use playtime_protocol::storage::StorageLimits;
use std::io::{Cursor, Read};
const GOLDEN: &[u8] = include_bytes!("fixtures/capture/reference.tar");
fn profile() -> CaptureProfile {
    CaptureProfile::prepare(
        CaptureLimits::default(),
        8 << 20,
        &StorageLimits::default(),
        ArchiveUsage::default(),
    )
    .unwrap()
}
fn parse(bytes: &[u8]) -> Result<ValidatedWorkspace> {
    validate_workspace(Cursor::new(bytes), &profile(), &CancellationToken::new())
}
fn checksum(block: &mut [u8]) {
    block[148..156].fill(b' ');
    let total: u64 = block.iter().map(|b| *b as u64).sum();
    block[148..156].copy_from_slice(format!("{total:06o}\0 ").as_bytes());
}
fn header(path: &str, kind: u8, size: u64, link: &str) -> Vec<u8> {
    let mut b = GOLDEN[..512].to_vec();
    b[..100].fill(0);
    b[..path.len()].copy_from_slice(path.as_bytes());
    b[156] = kind;
    b[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
    b[157..257].fill(0);
    b[157..157 + link.len()].copy_from_slice(link.as_bytes());
    checksum(&mut b);
    b
}
fn member(path: &str, kind: u8, body: &[u8], link: &str) -> Vec<u8> {
    let mut b = header(path, kind, body.len() as u64, link);
    b.extend_from_slice(body);
    b.resize(b.len().div_ceil(512) * 512, 0);
    b
}
fn archive(mut b: Vec<u8>) -> Vec<u8> {
    b.extend_from_slice(&[0; 1024]);
    b
}
fn pax_record(key: &str, value: &str) -> Vec<u8> {
    let payload = format!(" {key}={value}\n");
    let mut n = payload.len() + 1;
    loop {
        let next = payload.len() + n.to_string().len();
        if next == n {
            return format!("{n}{payload}").into_bytes();
        }
        n = next;
    }
}
#[test]
fn frozen_reference_preserves_bytes_metadata_and_metadata_only_links() {
    let w = parse(GOLDEN).unwrap();
    assert_eq!(w.tar(), GOLDEN);
    assert_eq!(
        w.sha256(),
        "e465b5958f9892de45a47cfe25802ce2330d1753fc8eb116a4e77dd1ae3b092e"
    );
    let entries = w.entries();
    assert_eq!(entries.len(), 6);
    assert_eq!(
        entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
        ["a.txt", "alias", "dir", "dir/é.txt", "outside", "run.sh"]
    );
    assert_eq!(
        entries.iter().map(|e| e.mode).collect::<Vec<_>>(),
        [0o640, 0o640, 0o750, 0o644, 0o777, 0o755]
    );
    assert_eq!(
        w.content(entries[0].content_slot.unwrap()),
        Some(b"alpha\n".as_slice())
    );
    assert_eq!(
        entries[0].sha256.as_deref(),
        Some("b6a98d9ce9a2d9149288fa3df42d377c3e42737afdcdaf714e33c0a100b51060")
    );
    assert_eq!(
        w.content(entries[3].content_slot.unwrap()),
        Some(b"beta\n".as_slice())
    );
    assert_eq!(entries[1].link.as_deref(), Some("a.txt"));
    assert!(entries[1].content_slot.is_none());
    assert_eq!(entries[4].link.as_deref(), Some("../../outside"));
    assert!(entries[4].content_slot.is_none());
    assert_eq!(w.index(), serde_json::to_vec(entries).unwrap());
}
#[test]
fn empty_and_long_unicode_names_and_inert_absolute_links() {
    let name = "é".repeat(100);
    let mut bytes = member("empty", b'0', b"", "");
    bytes.extend(member("outside", b'2', b"", "/etc/passwd"));
    bytes.extend(member(
        "././@PaxHeader",
        b'x',
        &pax_record("path", &name),
        "",
    ));
    bytes.extend(member("ignored", b'0', b"long", ""));
    let w = parse(&archive(bytes)).unwrap();
    assert_eq!(w.content(0), Some(b"".as_slice()));
    assert_eq!(w.entries()[2].path, name);
    assert_eq!(w.content(1), Some(b"long".as_slice()));
    assert_eq!(parse(&[0; 1024]).unwrap().entries().len(), 0);
}
#[test]
fn rejects_extensions_lengths_encodings_checksums_sizes_endings_and_tails() {
    let cases: Vec<Vec<u8>> = vec![
        b"999999999999999999999999 path=x\n".to_vec(),
        b"0 path=x\n".to_vec(),
        b"01 path=x\n".to_vec(),
        b"-1 path=x\n".to_vec(),
        b"11 path=x".to_vec(),
        b"11 path=x\n".to_vec(),
        b"10 path=x!".to_vec(),
        b"10 path=\xff\n".to_vec(),
        pax_record("size", "99999999999"),
        pax_record("GNU.sparse.map", "0,5"),
        pax_record("mtime", "0"),
        [pax_record("path", "a"), pax_record("path", "b")].concat(),
        pax_record("path", "../escape"),
        pax_record("path", "/escape"),
        pax_record("path", "a//b"),
        pax_record("path", "a\0b"),
        pax_record("path", &"a".repeat(4097)),
    ];
    for bad in cases {
        let b = [member("pax", b'x', &bad, ""), member("safe", b'0', b"", "")].concat();
        assert!(
            parse(&archive(b)).is_err(),
            "accepted {:?}",
            String::from_utf8_lossy(&bad)
        );
    }
    for kind in [b'g', b'L', b'K', b'S', b'3', b'4', b'6', 0] {
        assert!(parse(&archive(member("x", kind, b"", ""))).is_err());
    }
    for size in [67108865, 8589934591] {
        assert!(parse(&archive(header("x", b'0', size, ""))).is_err());
    }
    let p = member("pax", b'x', &pax_record("path", "a"), "");
    assert!(parse(&archive(p.clone())).is_err());
    assert!(
        parse(&archive(
            [p.clone(), p, member("a", b'0', b"", "")].concat()
        ))
        .is_err()
    );
    assert!(parse(&archive(member("pax", b'x', &vec![0; 16385], ""))).is_err());
    let mut b = GOLDEN.to_vec();
    b[0] ^= 1;
    assert!(parse(&b).is_err());
    for end in [0, 1, 511, 512, 513, 1023, 1024, 6143, 6144] {
        assert!(parse(&GOLDEN[..end]).is_err(), "{end}");
    }
    let mut b = GOLDEN.to_vec();
    b.push(0);
    assert!(parse(&b).is_err());
    let mut b = GOLDEN.to_vec();
    b[10000] = 1;
    assert!(parse(&b).is_err());
    assert!(parse(&[GOLDEN, GOLDEN].concat()).is_err());
    assert!(parse(&[0; 12288]).is_err());
    let mut b = header("x", b'0', 0, "");
    b[124] = 128;
    checksum(&mut b);
    assert!(parse(&archive(b)).is_err());
}
#[test]
fn rejects_path_conflicts_ancestors_and_hardlink_cycles_or_forward_targets() {
    for path in [
        "",
        "/absolute",
        "./a",
        "../a",
        "a//b",
        "a/./b",
        "a/../b",
        "a/",
        "a\0b",
    ] {
        assert!(
            parse(&archive(member(path, b'0', b"", ""))).is_err(),
            "{path}"
        );
    }
    for (kind, link) in [(b'0', ""), (b'2', "/outside"), (b'1', "parent")] {
        let bytes = [
            member("parent", kind, b"", link),
            member("parent/child", b'0', b"", ""),
        ]
        .concat();
        assert!(parse(&archive(bytes)).is_err());
    }
    for target in ["later", "self", "/outside", "../escape", "dir", "link"] {
        let bytes = [
            member("dir", b'5', b"", ""),
            member("link", b'2', b"", "/outside"),
            member("self", b'1', b"", target),
        ]
        .concat();
        assert!(parse(&archive(bytes)).is_err());
    }
    let bytes = [member("dir/", b'5', b"", ""), member("dir", b'5', b"", "")].concat();
    assert!(parse(&archive(bytes)).is_err());
    for kind in [b'1', b'2', b'5'] {
        assert!(parse(&archive(member("x", kind, b"payload", "y"))).is_err());
    }
    let mut b = member("x", b'0', b"a", "");
    b[513] = 1;
    assert!(parse(&archive(b)).is_err());
}
#[test]
fn capture_and_archive_budgets_are_checked_independently_with_overflow_rejected() {
    for limits in [
        CaptureLimits {
            entries: 5,
            ..CaptureLimits::default()
        },
        CaptureLimits {
            file_bytes: 5,
            ..CaptureLimits::default()
        },
        CaptureLimits {
            retained_bytes: 10,
            ..CaptureLimits::default()
        },
        CaptureLimits {
            index_bytes: 20,
            ..CaptureLimits::default()
        },
    ] {
        let p = CaptureProfile::prepare(
            limits,
            8 << 20,
            &StorageLimits::default(),
            ArchiveUsage::default(),
        )
        .unwrap();
        assert!(validate_workspace(GOLDEN, &p, &CancellationToken::new()).is_err());
    }
    for usage in [
        ArchiveUsage {
            content_bytes: 512 * 1024 * 1024 - 10240,
            ..ArchiveUsage::default()
        },
        ArchiveUsage {
            references: 9997,
            ..ArchiveUsage::default()
        },
    ] {
        let p = CaptureProfile::prepare(
            CaptureLimits::default(),
            8 << 20,
            &StorageLimits::default(),
            usage,
        )
        .unwrap();
        assert!(validate_workspace(GOLDEN, &p, &CancellationToken::new()).is_err());
    }
    for usage in [
        ArchiveUsage {
            content_bytes: u64::MAX,
            ..ArchiveUsage::default()
        },
        ArchiveUsage {
            references: u64::MAX,
            ..ArchiveUsage::default()
        },
        ArchiveUsage {
            events: u64::MAX,
            ..ArchiveUsage::default()
        },
        ArchiveUsage {
            log_bytes: u64::MAX,
            ..ArchiveUsage::default()
        },
    ] {
        assert!(
            CaptureProfile::prepare(
                CaptureLimits::default(),
                8 << 20,
                &StorageLimits::default(),
                usage
            )
            .is_err()
        );
    }
    assert_eq!(profile().limits().file_bytes, 8 << 20);
    assert_eq!(profile().limits().retained_bytes, 8 << 20);
    let storage = StorageLimits {
        content_bytes: 10240,
        manifest_bytes: 1024,
        ..StorageLimits::default()
    };
    let p = CaptureProfile::prepare(
        CaptureLimits::default(),
        8 << 20,
        &storage,
        ArchiveUsage::default(),
    )
    .unwrap();
    assert_eq!(p.limits().tar_bytes, 10240);
    assert!(validate_workspace(&[0u8; 11264][..], &p, &CancellationToken::new()).is_err());
}
#[test]
fn empty_workspace_index_obeys_requested_and_storage_limits() {
    for limit in [1, 2] {
        for (limits, storage) in [
            (
                CaptureLimits {
                    index_bytes: limit,
                    ..CaptureLimits::default()
                },
                StorageLimits::default(),
            ),
            (
                CaptureLimits::default(),
                StorageLimits {
                    manifest_bytes: limit,
                    ..StorageLimits::default()
                },
            ),
        ] {
            let p = CaptureProfile::prepare(limits, 8 << 20, &storage, ArchiveUsage::default())
                .unwrap();
            let result = validate_workspace(&[0u8; 10240][..], &p, &CancellationToken::new());
            if limit == 1 {
                assert_eq!(result.map(|_| ()).unwrap_err().failure, Failure::Capacity);
            } else {
                let workspace = result.unwrap();
                assert!(workspace.entries().is_empty());
                assert_eq!(workspace.index(), b"[]");
            }
        }
    }
}

struct Fragmented<'a> {
    bytes: &'a [u8],
    cancel: CancellationToken,
    at: usize,
    stop: usize,
}
impl Read for Fragmented<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = out.len().min(7).min(self.bytes.len() - self.at);
        out[..n].copy_from_slice(&self.bytes[self.at..self.at + n]);
        self.at += n;
        if self.at >= self.stop {
            self.cancel.cancel();
        }
        Ok(n)
    }
}
#[test]
fn streaming_parser_handles_short_reads_and_cancellation_at_each_stage() {
    let token = CancellationToken::new();
    let reader = Fragmented {
        bytes: GOLDEN,
        cancel: token.clone(),
        at: 0,
        stop: usize::MAX,
    };
    assert_eq!(
        validate_workspace(reader, &profile(), &token)
            .unwrap()
            .sha256(),
        parse(GOLDEN).unwrap().sha256()
    );
    for stop in [0, 10, 512, 520, 2000, 4000, 7000, 9999] {
        let token = CancellationToken::new();
        let reader = Fragmented {
            bytes: GOLDEN,
            cancel: token.clone(),
            at: 0,
            stop,
        };
        assert_eq!(
            validate_workspace(reader, &profile(), &token)
                .unwrap_err()
                .failure,
            Failure::Cancelled
        );
    }
}

#[test]
fn binary_bodies_are_retained_and_storage_reference_limits_bound_metadata() {
    let bytes = archive(member("binary", b'0', b"\xff\x00\x80\n", ""));
    let w = parse(&bytes).unwrap();
    assert_eq!(w.content(0), Some(b"\xff\x00\x80\n".as_slice()));
    let storage = StorageLimits {
        references: 3,
        ..StorageLimits::default()
    };
    let p = CaptureProfile::prepare(
        CaptureLimits::default(),
        8 << 20,
        &storage,
        ArchiveUsage::default(),
    )
    .unwrap();
    assert_eq!(p.limits().entries, 3);
    let directories = archive(
        [
            member("a", b'5', b"", ""),
            member("b", b'5', b"", ""),
            member("c", b'5', b"", ""),
            member("d", b'5', b"", ""),
        ]
        .concat(),
    );
    assert!(validate_workspace(directories.as_slice(), &p, &CancellationToken::new()).is_err());
}

#[test]
fn pax_linkpaths_preserve_long_inert_targets_and_earlier_hardlink_references() {
    let name = "a".repeat(200);
    let target = format!("../../{}", "escape".repeat(40));
    let mut regular = member("placeholder", b'0', b"", "");
    regular[100..108].copy_from_slice(b"0006755\0");
    checksum(&mut regular[..512]);
    let raw = archive(
        [
            member("pax", b'x', &pax_record("path", &name), ""),
            regular,
            member("pax", b'x', &pax_record("linkpath", &target), ""),
            member("b", b'2', b"", "placeholder"),
            member("pax", b'x', &pax_record("linkpath", &name), ""),
            member("c", b'1', b"", "placeholder"),
        ]
        .concat(),
    );
    let w = parse(&raw).unwrap();
    assert_eq!(w.entries()[0].mode, 0o6755);
    assert_eq!(w.entries()[1].link.as_deref(), Some(target.as_str()));
    assert_eq!(w.entries()[2].link.as_deref(), Some(name.as_str()));
    for target in [
        "../escape".to_owned(),
        "future".to_owned(),
        "a".repeat(4097),
    ] {
        let bytes = archive(
            [
                member("pax", b'x', &pax_record("linkpath", &target), ""),
                member("c", b'1', b"", "a"),
            ]
            .concat(),
        );
        assert!(parse(&bytes).is_err());
    }
}
