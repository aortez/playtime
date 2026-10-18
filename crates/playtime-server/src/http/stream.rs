use super::*;
use axum::body::Bytes;
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::{
    sync::{OwnedSemaphorePermit, mpsc, watch},
    time::{Instant, timeout_at},
};
use tokio_util::io::ReaderStream;

struct Frame {
    bytes: Bytes,
    _budget: OwnedSemaphorePermit,
}
fn queue(tx: &mpsc::Sender<Frame>, budget: &Arc<Semaphore>, bytes: Vec<u8>) -> Result<()> {
    let permit = budget
        .clone()
        .try_acquire_many_owned(
            bytes
                .len()
                .try_into()
                .map_err(|_| ApiError::new("evidence_limit_exceeded"))?,
        )
        .map_err(|_| ApiError::new("too_many_observers"))?;
    tx.try_send(Frame {
        bytes: Bytes::from(bytes),
        _budget: permit,
    })
    .map_err(|_| ApiError::new("too_many_observers"))
}
fn notice(kind: &str, value: &impl Serialize) -> Result<Vec<u8>> {
    let data = bounded_json(value, 8192)?;
    Ok(format!(
        "event: {kind}\ndata: {}\n\n",
        String::from_utf8(data).unwrap()
    )
    .into_bytes())
}

pub(super) async fn events(
    server: Arc<Server>,
    run: RunId,
    cursor: Option<EventCursor>,
    request_id: Option<RequestId>,
) -> Result<Response> {
    let permit = server
        .streams
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::new("too_many_observers"))?;
    // Subscribe first, then validate/read. A revision during either operation
    // remains pending on this receiver; no filesystem notification is trusted.
    let mut live = server.application.subscribe(run.clone()).await?;
    let signal = live.borrow_and_update().clone();
    let first = server
        .application
        .history(
            run.clone(),
            cursor,
            true,
            1,
            server.limits().event_bytes + 1,
        )
        .await?;
    let (tx, mut rx) = mpsc::channel::<Frame>(server.limits().stream_pending_records);
    let budget = Arc::new(Semaphore::new(server.limits().stream_pending_bytes));
    let producer = server.clone();
    let task = tokio::spawn(async move {
        let result = follow(&producer, run, first, signal, &mut live, &tx, &budget).await;
        if let Err(error) = result {
            let mut safe = ApiError::new(error.code()).0;
            safe.request_id = request_id;
            if let Ok(bytes) = notice("stream_error", &safe) {
                let _ = queue(&tx, &budget, bytes);
            }
        }
    });
    let shutdown = server.shutdown.clone();
    let stream = async_stream::stream! {
        let _permit = permit;
        // Aborting on body drop also covers clients disappearing between headers
        // and their first frame. Experimental work is never owned by this task.
        struct Abort(tokio::task::AbortHandle);
        impl Drop for Abort { fn drop(&mut self) { self.0.abort(); } }
        let _abort = Abort(task.abort_handle());
        loop {
            let frame = tokio::select! { _ = shutdown.cancelled() => None, frame = rx.recv() => frame };
            let Some(frame) = frame else { break; };
            yield Ok::<_, io::Error>(frame.bytes);
            drop(frame._budget);
        }
    };
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream))
        .unwrap())
}
async fn follow(
    server: &Arc<Server>,
    run: RunId,
    mut history: History,
    mut signal: crate::application::LiveState,
    live: &mut watch::Receiver<crate::application::LiveState>,
    tx: &mpsc::Sender<Frame>,
    budget: &Arc<Semaphore>,
) -> Result<()> {
    let mut heartbeat =
        tokio::time::interval(Duration::from_secs(server.limits().heartbeat_seconds));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    heartbeat.tick().await;
    loop {
        let page = event_page(history, server.limits())?;
        for event in &page.events {
            let cursor = EventCursor::new(page.frontier.clone(), event.sequence.get())
                .unwrap()
                .encode()
                .unwrap();
            let json = bounded_json(event, server.limits().event_bytes)?;
            let bytes = format!(
                "event: record\nid: {cursor}\ndata: {}\n\n",
                String::from_utf8(json).unwrap()
            )
            .into_bytes();
            if bytes.len() > server.limits().event_bytes + server.limits().sse_framing_bytes {
                return Err(ApiError::new("evidence_limit_exceeded"));
            }
            queue(tx, budget, bytes)?;
        }
        let cursor = EventCursor::decode(&page.resume_cursor).unwrap();
        if !page.has_more {
            if let Some(end) = signal.end
                && end.frontier == page.frontier
            {
                queue(tx, budget, notice("stream_end", &end)?)?;
                return Ok(());
            }
            loop {
                tokio::select! {
                    _ = tx.closed() => return Ok(()),
                    _ = server.shutdown.cancelled() => return Ok(()),
                    changed = live.changed() => {
                        changed.map_err(|_| ApiError::new("ownership_unresolved"))?;
                        break;
                    }
                    _ = heartbeat.tick() => queue(tx, budget, b": heartbeat\n\n".to_vec())?,
                }
            }
        }
        signal = live.borrow_and_update().clone();
        let read = tokio::time::timeout(
            Duration::from_secs(server.limits().handler_seconds),
            server.application.history(
                run.clone(),
                Some(cursor),
                true,
                1,
                server.limits().event_bytes + 1,
            ),
        );
        tokio::pin!(read);
        history = loop {
            tokio::select! {
                _ = tx.closed() => return Ok(()),
                _ = server.shutdown.cancelled() => return Ok(()),
                _ = heartbeat.tick() => queue(tx, budget, b": heartbeat\n\n".to_vec())?,
                result = &mut read => break result.map_err(|_| ApiError::new("request_timeout"))??,
            }
        };
    }
}

pub(super) async fn download(server: Arc<Server>, run: RunId, id: ArtifactId) -> Result<Response> {
    let permit = server
        .downloads
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::new("too_many_observers"))?;
    let download = server.application.download(run, id.clone()).await?;
    if download.artifact.artifact_id != id {
        return Err(ApiError::new("content_mismatch"));
    }
    let content = download.artifact.content;
    if content.size_bytes > server.limits().artifact_bytes {
        return Err(ApiError::new("evidence_limit_exceeded"));
    }
    let length = content.size_bytes;
    let hash = content.sha256.to_string();
    let digest_bytes: Vec<_> = (0..64)
        .step_by(2)
        .map(|i| u8::from_str_radix(&hash[i..i + 2], 16).unwrap())
        .collect();
    let content_digest = format!("sha-256=:{}:", STANDARD.encode(digest_bytes));
    let deadline = Instant::now() + Duration::from_secs(server.limits().handler_seconds);
    let stream = async_stream::try_stream! {
        let _permit = permit;
        let mut source = ReaderStream::with_capacity(download.reader, 64 << 10);
        let mut count = 0u64;
        let mut hasher = Sha256::new();
        loop {
            let next = tokio::select! {
                _ = server.shutdown.cancelled() => Err(io::Error::other("service stopped")),
                result = timeout_at(deadline, source.next()) => result.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "download deadline exceeded")),
            }?;
            let Some(bytes) = next else { break; };
            let bytes = bytes?;
            count = count.checked_add(bytes.len() as u64).ok_or_else(|| io::Error::other("content length overflow"))?;
            if count > length { Err(io::Error::other("content length mismatch"))?; }
            hasher.update(&bytes);
            // Verify before yielding the final bytes. A mismatch cannot look
            // like a successfully completed Content-Length body.
            if count == length && hasher.clone().finalize().iter().map(|b| format!("{b:02x}")).collect::<String>() != hash { Err(io::Error::other("content digest mismatch"))?; }
            if count == length {
                let tail = tokio::select! {
                    _ = server.shutdown.cancelled() => Err(io::Error::other("service stopped")),
                    result = timeout_at(deadline, source.next()) => result.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "download deadline exceeded")),
                }?;
                if tail.is_some() { Err(io::Error::other("content length mismatch"))?; }
                yield bytes;
                break;
            }
            yield bytes;
        }
        if count != length || hasher.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>() != hash { Err(io::Error::other("content mismatch"))?; }
    };
    let stream: std::pin::Pin<Box<dyn futures_util::Stream<Item = io::Result<Bytes>> + Send>> =
        Box::pin(stream);
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, length)
        .header("content-digest", content_digest)
        .header("x-playtime-sha256", content.sha256.as_str())
        .header("accept-ranges", "none")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{id}\""),
        )
        .body(Body::from_stream(stream))
        .unwrap())
}
