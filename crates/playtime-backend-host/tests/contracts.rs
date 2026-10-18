use playtime_backend_guard::{digest, ports::Stamp, wire::*};
use playtime_backend_host::{
    clock::{check_send, map_deadline},
    deadline::Register,
    protocol::*,
};
use playtime_protocol::worker::ControlId;
fn id(n: u8) -> ControlId {
    ControlId::new(format!("{n:032x}")).unwrap()
}
fn backend() -> BackendIdentity {
    BackendIdentity {
        backend_id: id(1),
        guard_id: id(2),
        boot_id: id(3),
        unit: "ollama.service".into(),
        invocation_id: id(4),
        cgroup: "/system.slice/ollama.service".into(),
        generation: 1,
        profile_sha256: digest(b"profile"),
    }
}
fn lease() -> LeaseSpec {
    LeaseSpec {
        operation_id: id(5),
        lease_id: id(6),
        backend: backend(),
        service_id: id(7),
        run_id: playtime_core::RunId::new("fixture-run").unwrap(),
        manifest_sha256: digest(b"manifest"),
        worker_id: id(9),
        grant_id: id(10),
        owner_revision: 1,
        execution_deadline_ms: 120_000,
        cleanup_deadline_ms: 300_000,
    }
}
fn request() -> RequestSpec {
    RequestSpec {
        lease_id: id(6),
        generation: 1,
        ordinal: 1,
        request_id: id(11),
        effect_id: id(12),
        body_sha256: digest(b"x"),
        body_bytes: 1,
        original_max_tokens: 7,
        deadline_ms: 100_000,
    }
}
fn reply() -> ProfileReply {
    ProfileReply {
        version: 1,
        nonce: id(13),
        local_boot: id(14),
        local_send_ms: 10_000,
        host_ms: 9_000_000,
        backend: backend(),
        lock_sha256: digest(b"lock"),
        settings_sha256: digest(b"settings"),
    }
}
#[test]
fn mapping_uses_receive_bound_not_latency_guess_or_epoch() {
    let sent = Stamp {
        boot_id: id(14),
        ms: 10_000,
    };
    let received = Stamp {
        boot_id: id(14),
        ms: 10_800,
    };
    let m = map_deadline(&sent, &received, &id(13), &reply(), 130_000).unwrap();
    assert_eq!(m.host_deadline_ms, 9_118_200); // independent integer oracle
    assert!(
        check_send(
            &m,
            &Stamp {
                boot_id: id(14),
                ms: 128_999
            }
        )
        .is_ok()
    );
    assert!(
        check_send(
            &m,
            &Stamp {
                boot_id: id(14),
                ms: 129_000
            }
        )
        .is_err()
    );
    // Arbitrarily asymmetric transit remains conservative: H can be anywhere
    // in [L0,L1] and offset is unrelated to the local epoch.
    for offset in [0, 1_000_000, 9_000_000_000] {
        for transit in [0, 400, 800] {
            let mut r = reply();
            r.host_ms = offset + 10_000 + transit;
            let mapped = map_deadline(&sent, &received, &id(13), &r, 130_000).unwrap();
            assert!(mapped.host_deadline_ms < offset + 130_000);
        }
    }
}
#[test]
fn mapping_rejects_boot_nonce_horizon_pause_overflow_and_expiry() {
    let sent = Stamp {
        boot_id: id(14),
        ms: 10_000,
    };
    let received = Stamp {
        boot_id: id(14),
        ms: 10_001,
    };
    for d in [10_000, 11_001, 310_001, u64::MAX] {
        assert!(map_deadline(&sent, &received, &id(13), &reply(), d).is_err());
    }
    assert!(
        map_deadline(
            &sent,
            &Stamp {
                boot_id: id(14),
                ms: 11_001
            },
            &id(13),
            &reply(),
            130_000
        )
        .is_err()
    );
    assert!(
        map_deadline(
            &sent,
            &Stamp {
                boot_id: id(15),
                ms: 10_001
            },
            &id(13),
            &reply(),
            130_000
        )
        .is_err()
    );
    assert!(map_deadline(&sent, &received, &id(15), &reply(), 130_000).is_err());
    let mut r = reply();
    r.host_ms = u64::MAX - 1;
    assert!(map_deadline(&sent, &received, &id(13), &r, 130_000).is_err());
    let m = map_deadline(&sent, &received, &id(13), &reply(), 130_000).unwrap();
    assert!(
        check_send(
            &m,
            &Stamp {
                boot_id: id(14),
                ms: 500_000
            }
        )
        .is_err()
    ); // suspend counts
    let mut forged = m;
    forged.host_deadline_ms += 1;
    assert!(check_send(&forged, &received).is_err());
}
#[test]
fn host_version_is_separate_strict_and_not_authority() {
    let bytes = serde_json::to_vec(&reply()).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["version"], 1);
    assert!(value.get("capability").is_none());
    let mut extra = value;
    extra["terminated"] = true.into();
    assert!(serde_json::from_value::<ProfileReply>(extra).is_err());
    assert_eq!(playtime_protocol::backend_guard::VERSION, 1);
}
#[test]
fn deadline_register_fences_competitors_retries_caps_and_retired_operations() {
    let mut r = Register::new(backend());
    let l = lease();
    let q = request();
    r.arm_lease(&l, 0).unwrap();
    r.arm_lease(&l, 10).unwrap();
    let mut contender = l.clone();
    contender.lease_id = id(22);
    contender.operation_id = id(23);
    assert_eq!(r.arm_lease(&contender, 10), Err(Failure::Busy));
    let mut extended = l.clone();
    extended.execution_deadline_ms += 1;
    assert!(r.arm_lease(&extended, 10).is_err());
    r.arm_request(&q, 10).unwrap();
    r.arm_request(&q, 20).unwrap();
    let mut altered = q.clone();
    altered.original_max_tokens += 1;
    assert!(r.arm_request(&altered, 20).is_err());
    assert_eq!(r.deadline(), Some(100_000));
    assert!(r.release(&l.lease_id, false).is_err());
    r.terminal(&q, 30).unwrap();
    assert_eq!(r.deadline(), Some(120_000));
    assert!(r.arm_request(&q, 40).is_err());
    r.release(&l.lease_id, false).unwrap();
    assert!(r.arm_lease(&l, 50).is_err());
}
#[test]
fn first_stop_survives_later_control_and_never_renews_cleanup() {
    let mut r = Register::new(backend());
    r.arm_lease(&lease(), 0).unwrap();
    r.arm_request(&request(), 1).unwrap();
    let first = r
        .select_stop(StopReason::Deadline, 100_500, None)
        .unwrap()
        .unwrap();
    assert_eq!(first.decided_ms, 100_000);
    assert_eq!(first.supervisor_deadline_ms, 190_000);
    assert_eq!(first.observation_deadline_ms, 220_000);
    let mut later = first.clone();
    later.operation_id = id(40);
    later.decided_ms = 110_000;
    later.reason = StopReason::Revoked;
    assert_eq!(
        r.select_stop(StopReason::Revoked, 110_000, Some(&later))
            .unwrap()
            .unwrap(),
        first
    );
    assert!(r.terminal(&request(), 110_000).is_err());
    assert!(r.release(&id(6), false).is_err());
    r.release(&id(6), true).unwrap();
    assert!(r.arm_lease(&lease(), 110_000).is_err());
}
#[tokio::test]
async fn kernel_boottime_timer_is_absolute_and_expired_arrival_is_immediate() {
    use playtime_backend_guard::ports::{Clock, SystemClock};
    let clock = SystemClock::new().unwrap();
    let timer = playtime_backend_host::deadline::AbsoluteTimer::new().unwrap();
    timer.arm(clock.now().ms.saturating_sub(1)).unwrap();
    tokio::time::timeout(std::time::Duration::from_millis(100), timer.expired())
        .await
        .unwrap()
        .unwrap();
    assert!(timer.arm(0).is_err());
}
