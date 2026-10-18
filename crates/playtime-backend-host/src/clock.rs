//! Conservative short-horizon mapping, never wall time or a renewable budget.
use crate::{Result, protocol::*, wire::Failure};
use playtime_backend_guard::ports::Stamp;
use playtime_protocol::worker::ControlId;

pub fn map_deadline(
    sent: &Stamp,
    received: &Stamp,
    nonce: &ControlId,
    reply: &ProfileReply,
    deadline: u64,
) -> Result<DeadlineMapping> {
    let rtt = received.ms.checked_sub(sent.ms).ok_or(Failure::Expired)?;
    let horizon = deadline.checked_sub(sent.ms).ok_or(Failure::Expired)?;
    if sent.boot_id != received.boot_id
        || reply.local_boot != sent.boot_id
        || reply.local_send_ms != sent.ms
        || &reply.nonce != nonce
        || reply.version != VERSION
        || rtt > MAX_RTT_MS
        || horizon > MAX_HORIZON_MS
        || reply.backend.validate().is_err()
    {
        return Err(Failure::Invalid);
    }
    let remaining = deadline
        .checked_sub(received.ms)
        .and_then(|v| v.checked_sub(MARGIN_MS))
        .filter(|v| *v > 0)
        .ok_or(Failure::Expired)?;
    let host_deadline_ms = reply
        .host_ms
        .checked_add(remaining)
        .ok_or(Failure::Invalid)?;
    Ok(DeadlineMapping {
        version: VERSION,
        nonce: nonce.clone(),
        local_boot: sent.boot_id.clone(),
        backend: reply.backend.clone(),
        local_send_ms: sent.ms,
        local_receive_ms: received.ms,
        host_sample_ms: reply.host_ms,
        local_deadline_ms: deadline,
        host_deadline_ms,
    })
}
/// Recompute the original mapping and reject stale local delivery. Call just
/// before submission; persist the returned mapping with the original operation.
pub fn check_send(mapping: &DeadlineMapping, now: &Stamp) -> Result<()> {
    let reply = ProfileReply {
        version: mapping.version,
        nonce: mapping.nonce.clone(),
        local_boot: mapping.local_boot.clone(),
        local_send_ms: mapping.local_send_ms,
        host_ms: mapping.host_sample_ms,
        backend: mapping.backend.clone(),
        lock_sha256: playtime_backend_guard::digest(&[]),
        settings_sha256: playtime_backend_guard::digest(&[]),
    };
    let actual = map_deadline(
        &Stamp {
            boot_id: mapping.local_boot.clone(),
            ms: mapping.local_send_ms,
        },
        &Stamp {
            boot_id: mapping.local_boot.clone(),
            ms: mapping.local_receive_ms,
        },
        &mapping.nonce,
        &reply,
        mapping.local_deadline_ms,
    )?;
    if &actual != mapping
        || now.boot_id != mapping.local_boot
        || now.ms < mapping.local_receive_ms
        || now
            .ms
            .checked_add(MARGIN_MS)
            .is_none_or(|n| n >= mapping.local_deadline_ms)
    {
        return Err(Failure::Expired);
    }
    Ok(())
}
