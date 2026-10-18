use super::*;
use playtime_protocol::events::EventEnvelope;

#[derive(Clone, Debug, PartialEq)]
pub struct ObservedRecord {
    pub event: EventEnvelope,
    pub cursor: String,
}
#[derive(Clone, Debug, PartialEq)]
pub enum Observation {
    Record(ObservedRecord),
    End(StreamEnd),
    Disconnected,
}

#[derive(Clone, Debug)]
pub struct RetryPolicy {
    pub max_reconnects: u32,
    pub max_elapsed: Duration,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}
impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_reconnects: 5,
            max_elapsed: Duration::from_secs(120),
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(5),
        }
    }
}

#[derive(Default)]
struct Frame {
    event: String,
    id: Option<String>,
    data: String,
}
#[derive(Default)]
struct Parser {
    line: Vec<u8>,
    frame: Frame,
    bytes: usize,
    skip_lf: bool,
    first_line: bool,
}
impl Parser {
    fn new() -> Self {
        Self {
            first_line: true,
            ..Self::default()
        }
    }
    fn consume(&mut self, bytes: &[u8], limit: usize) -> Result<(usize, Option<Frame>)> {
        for (index, &byte) in bytes.iter().enumerate() {
            self.bytes += 1;
            if self.bytes > limit {
                return Err(Error::LimitExceeded);
            }
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if matches!(byte, b'\r' | b'\n') {
                self.skip_lf = byte == b'\r';
                let line = std::str::from_utf8(&self.line).map_err(|_| Error::InvalidResponse)?;
                let line = if self.first_line {
                    line.trim_start_matches('\u{feff}')
                } else {
                    line
                };
                self.first_line = false;
                if line.is_empty() {
                    let frame = std::mem::take(&mut self.frame);
                    self.bytes = 0;
                    self.line.clear();
                    if !frame.data.is_empty() || !frame.event.is_empty() || frame.id.is_some() {
                        return Ok((index + 1, Some(frame)));
                    }
                } else {
                    if !line.starts_with(':') {
                        let (field, value) = line.split_once(':').unwrap_or((line, ""));
                        let value = value.strip_prefix(' ').unwrap_or(value);
                        match field {
                            "event" => {
                                if !self.frame.event.is_empty() {
                                    return Err(Error::InvalidResponse);
                                }
                                self.frame.event = value.into();
                            }
                            "id" => {
                                if self.frame.id.is_some() || value.contains('\0') {
                                    return Err(Error::InvalidResponse);
                                }
                                self.frame.id = Some(value.into());
                            }
                            "data" => {
                                if !self.frame.data.is_empty() {
                                    self.frame.data.push('\n');
                                }
                                self.frame.data.push_str(value);
                            }
                            _ => (),
                        }
                    }
                    self.line.clear();
                }
            } else {
                self.line.push(byte);
            }
        }
        Ok((bytes.len(), None))
    }
}

pub struct EventStream {
    response: Response,
    parser: Parser,
    pending: Vec<u8>,
    offset: usize,
    run: RunId,
    acknowledged: Option<String>,
    delivered: Option<String>,
    limits: ApiLimits,
    ended: bool,
}
impl Client {
    pub async fn stream(
        &self,
        run: &RunId,
        cursor: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<EventStream> {
        if let Some(cursor) = cursor {
            let decoded = EventCursor::decode(cursor).map_err(|_| Error::InvalidResponse)?;
            if decoded.frontier.run_id != *run {
                return Err(Error::InvalidResponse);
            }
        }
        let mut request = self.request(Method::GET, &format!("runs/{run}/events/stream"));
        if let Some(cursor) = cursor {
            request = request.header("last-event-id", cursor);
        }
        // This bounds only the handshake. The body has an idle timeout, never a
        // 30-second whole-stream deadline.
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            result = timeout(Duration::from_secs(self.limits.handler_seconds), request.send()) => result.map_err(|_| Error::Timeout)?.map_err(transport)?,
        };
        self.check_response(&response)?;
        if !response.status().is_success() {
            let error: Result<serde_json::Value> = timeout(
                Duration::from_secs(self.limits.handler_seconds),
                read_json(response, self.limits.json_bytes),
            )
            .await
            .map_err(|_| Error::Timeout)?;
            return Err(error.err().unwrap_or(Error::InvalidResponse));
        }
        if response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            != Some("text/event-stream")
        {
            return Err(Error::InvalidResponse);
        }
        Ok(EventStream {
            response,
            parser: Parser::new(),
            pending: vec![],
            offset: 0,
            run: run.clone(),
            acknowledged: cursor.map(str::to_owned),
            delivered: None,
            limits: self.limits.clone(),
            ended: false,
        })
    }
    pub fn follow(&self, run: &RunId, acknowledged: Option<String>, policy: RetryPolicy) -> Follow {
        Follow {
            client: self.clone(),
            run: run.clone(),
            acknowledged,
            policy,
            stream: None,
            reconnects: 0,
            started: Instant::now(),
            needs_reconnect: false,
            delivered: None,
            ended: false,
        }
    }
}
impl EventStream {
    pub fn acknowledged_cursor(&self) -> Option<&str> {
        self.acknowledged.as_deref()
    }
    pub fn acknowledge(&mut self, record: &ObservedRecord) -> Result<()> {
        if self.delivered.as_deref() != Some(record.cursor.as_str()) {
            return Err(Error::InvalidResponse);
        }
        self.acknowledged = self.delivered.take();
        Ok(())
    }
    pub async fn next(&mut self, cancel: &CancellationToken) -> Result<Observation> {
        if self.delivered.is_some() {
            return Err(Error::UnacknowledgedRecord);
        }
        if self.ended {
            return Ok(Observation::Disconnected);
        }
        loop {
            let (consumed, frame) = self.parser.consume(
                &self.pending[self.offset..],
                self.limits.event_bytes + self.limits.sse_framing_bytes,
            )?;
            self.offset += consumed;
            if let Some(frame) = frame {
                return self.observation(frame);
            }
            self.pending.clear();
            self.offset = 0;
            let chunk = tokio::select! {
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                result = timeout(Duration::from_secs(self.limits.stream_idle_seconds), self.response.chunk()) => result.map_err(|_| Error::Timeout)?.map_err(transport)?,
            };
            let Some(chunk) = chunk else {
                self.ended = true;
                // Discard a partial frame. EOF is transport loss, never a
                // cursor acknowledgment or experimental completion.
                return Ok(Observation::Disconnected);
            };
            if chunk.len() > self.limits.stream_pending_bytes {
                return Err(Error::LimitExceeded);
            }
            self.pending.extend_from_slice(&chunk);
        }
    }
    fn observation(&mut self, frame: Frame) -> Result<Observation> {
        match frame.event.as_str() {
            "record" => {
                if frame.data.len() > self.limits.event_bytes {
                    return Err(Error::LimitExceeded);
                }
                let cursor = frame.id.ok_or(Error::InvalidResponse)?;
                let position = EventCursor::decode(&cursor).map_err(|_| Error::InvalidResponse)?;
                let event = decode_event(&frame.data)
                    .map_err(|_| Error::InvalidResponse)?
                    .envelope()
                    .clone();
                if event.run_id != self.run
                    || position.frontier.run_id != self.run
                    || position.after_sequence != event.sequence.get()
                {
                    return Err(Error::InvalidResponse);
                }
                let previous = self
                    .acknowledged
                    .as_deref()
                    .map(EventCursor::decode)
                    .transpose()
                    .map_err(|_| Error::InvalidResponse)?;
                if let Some(previous) = previous {
                    if previous.frontier.manifest_sha256 != position.frontier.manifest_sha256
                        || Some(position.after_sequence) != previous.after_sequence.checked_add(1)
                    {
                        return Err(Error::InvalidResponse);
                    }
                } else if event.sequence.get() != 1 {
                    return Err(Error::InvalidResponse);
                }
                self.delivered = Some(cursor.clone());
                Ok(Observation::Record(ObservedRecord { event, cursor }))
            }
            "stream_end" => {
                if frame.id.is_some() {
                    return Err(Error::InvalidResponse);
                }
                let end: StreamEnd =
                    serde_json::from_str(&frame.data).map_err(|_| Error::InvalidResponse)?;
                let previous = self
                    .acknowledged
                    .as_deref()
                    .map(EventCursor::decode)
                    .transpose()
                    .map_err(|_| Error::InvalidResponse)?;
                if end.frontier.run_id != self.run
                    || previous.as_ref().is_some_and(|p| {
                        p.frontier.manifest_sha256 != end.frontier.manifest_sha256
                            || p.after_sequence != end.frontier.prefix.sequence
                    })
                    || (previous.is_none() && end.frontier.prefix.sequence != 0)
                {
                    return Err(Error::InvalidResponse);
                }
                self.ended = true;
                Ok(Observation::End(end))
            }
            "stream_error" => {
                if frame.id.is_some() {
                    return Err(Error::InvalidResponse);
                }
                let response: ErrorResponse =
                    serde_json::from_str(&frame.data).map_err(|_| Error::InvalidResponse)?;
                self.ended = true;
                Err(Error::Api {
                    status: 200,
                    response,
                })
            }
            _ => Err(Error::InvalidResponse),
        }
    }
}

/// Reconnects only from the explicitly acknowledged position. Applying a record
/// and persisting its cursor is the caller's responsibility; deduplicate by
/// (run, sequence) when a crash interrupts that transaction.
pub struct Follow {
    client: Client,
    run: RunId,
    acknowledged: Option<String>,
    policy: RetryPolicy,
    stream: Option<EventStream>,
    reconnects: u32,
    started: Instant,
    needs_reconnect: bool,
    delivered: Option<String>,
    ended: bool,
}
impl Follow {
    pub fn acknowledged_cursor(&self) -> Option<&str> {
        self.acknowledged.as_deref()
    }
    pub fn acknowledge(&mut self, record: &ObservedRecord) -> Result<()> {
        if self.delivered.as_deref() != Some(record.cursor.as_str()) {
            return Err(Error::InvalidResponse);
        }
        self.stream
            .as_mut()
            .ok_or(Error::InvalidResponse)?
            .acknowledge(record)?;
        self.acknowledged = self.delivered.take();
        Ok(())
    }
    pub async fn next(&mut self, cancel: &CancellationToken) -> Result<Observation> {
        if self.delivered.is_some() {
            return Err(Error::UnacknowledgedRecord);
        }
        if self.ended {
            return Ok(Observation::Disconnected);
        }
        loop {
            if self.stream.is_none() {
                if self.needs_reconnect {
                    if self.reconnects >= self.policy.max_reconnects
                        || self.started.elapsed() >= self.policy.max_elapsed
                    {
                        return Err(Error::RetryBudgetExhausted);
                    }
                    let delay = self
                        .policy
                        .initial_backoff
                        .saturating_mul(2u32.saturating_pow(self.reconnects.min(31)))
                        .min(self.policy.max_backoff)
                        .min(
                            self.policy
                                .max_elapsed
                                .saturating_sub(self.started.elapsed()),
                        );
                    tokio::select! { _ = cancel.cancelled() => return Err(Error::Cancelled), _ = tokio::time::sleep(delay) => () }
                    self.reconnects += 1;
                }
                let connect = self
                    .client
                    .stream(&self.run, self.acknowledged.as_deref(), cancel);
                let result = if self.needs_reconnect {
                    let remaining = self
                        .policy
                        .max_elapsed
                        .saturating_sub(self.started.elapsed());
                    if remaining.is_zero() {
                        return Err(Error::RetryBudgetExhausted);
                    }
                    timeout(remaining, connect)
                        .await
                        .map_err(|_| Error::RetryBudgetExhausted)?
                } else {
                    connect.await
                };
                match result {
                    Ok(stream) => self.stream = Some(stream),
                    Err(error) if error.retryable() => {
                        self.needs_reconnect = true;
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            }
            match self.stream.as_mut().unwrap().next(cancel).await {
                Ok(Observation::Disconnected) => {
                    self.stream = None;
                    self.needs_reconnect = true;
                }
                Err(error) if error.retryable() => {
                    self.stream = None;
                    self.needs_reconnect = true;
                }
                Ok(Observation::Record(record)) => {
                    self.delivered = Some(record.cursor.clone());
                    return Ok(Observation::Record(record));
                }
                Ok(end @ Observation::End(_)) => {
                    self.ended = true;
                    return Ok(end);
                }
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frames_survive_every_byte_boundary_and_all_sse_line_endings() {
        for separator in ["\n", "\r\n", "\r"] {
            let bytes = format!("\u{feff}: hello{separator}{separator}event: record{separator}id: cursor{separator}data: {{\"text\":\"é💥\"}}{separator}{separator}").into_bytes();
            for split in 0..=bytes.len() {
                let mut parser = Parser::new();
                let mut frames = vec![];
                for chunk in [&bytes[..split], &bytes[split..]] {
                    let mut offset = 0;
                    while offset < chunk.len() {
                        let (used, frame) = parser.consume(&chunk[offset..], 2048).unwrap();
                        offset += used;
                        if let Some(frame) = frame {
                            frames.push(frame);
                        }
                    }
                }
                assert_eq!(frames.len(), 1);
                assert_eq!(frames[0].event, "record");
                assert_eq!(frames[0].id.as_deref(), Some("cursor"));
                assert_eq!(frames[0].data, "{\"text\":\"é💥\"}");
            }
        }
    }
    #[test]
    fn unterminated_lines_are_bounded_and_do_not_become_frames() {
        let mut parser = Parser::new();
        assert!(parser.consume(b"data: abc", 10).unwrap().1.is_none());
        assert!(matches!(
            parser.consume(b"def", 10),
            Err(Error::LimitExceeded)
        ));
        let mut parser = Parser::new();
        assert!(
            parser
                .consume(b"event: record\nid: a\nid: b\n\n", 100)
                .is_err()
        );
    }
}
