//! A scripted loopback Strata server on its own threads. A caller that runs a
//! synchronous driver can't serve requests from its own runtime, so this one
//! never shares one.
use super::metadata;
use serde_json::Value;
use std::{
    collections::{BTreeMap, VecDeque},
    io::{ErrorKind, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

/// What one `/v1/responses` request gets.
#[derive(Clone)]
pub enum Reply {
    /// These SSE bytes, then the connection closes.
    Events(Vec<u8>),
    /// These bytes, then the connection stays open until the client leaves.
    Stall(Vec<u8>),
    /// Each frame after waiting `gap`, then the connection closes.
    Paced(Vec<Vec<u8>>, Duration),
}

pub struct State {
    pub props: Value,
    pub status: Value,
    pub settings: Value,
    /// Replies in request order. A request with none left gets HTTP 500.
    pub replies: VecDeque<Reply>,
    /// Status documents the server reports from the time that many
    /// `/v1/responses` requests have arrived, as after a restart.
    pub statuses: BTreeMap<usize, Value>,
    /// Every request in arrival order: its path and a POST's JSON body.
    pub seen: Vec<(String, Value)>,
}

pub struct Server {
    /// The `/v1` endpoint a client connects to.
    pub endpoint: String,
    pub state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}
impl Server {
    pub fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let (props, status, settings) = metadata();
        let state = Arc::new(Mutex::new(State {
            props,
            status,
            settings,
            replies: replies.into_iter().collect(),
            statuses: BTreeMap::new(),
            seen: vec![],
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (shared, stopping) = (state.clone(), stop.clone());
        let accept = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        let (shared, stopping) = (shared.clone(), stopping.clone());
                        std::thread::spawn(move || serve(socket, &shared, &stopping));
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(e) => panic!("fixture accept failed: {e}"),
                }
            }
        });
        Self {
            endpoint,
            state,
            stop,
            accept: Some(accept),
        }
    }
    /// The paths requested so far, in order.
    pub fn paths(&self) -> Vec<String> {
        let state = self.state.lock().unwrap();
        state.seen.iter().map(|(path, _)| path.clone()).collect()
    }
    /// The JSON bodies of every `/v1/responses` request so far.
    pub fn posts(&self) -> Vec<Value> {
        let state = self.state.lock().unwrap();
        state
            .seen
            .iter()
            .filter(|(path, _)| path == "/v1/responses")
            .map(|(_, body)| body.clone())
            .collect()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

fn serve(mut socket: TcpStream, state: &Mutex<State>, stop: &AtomicBool) {
    socket.set_nonblocking(false).unwrap();
    let (mut bytes, mut chunk) = (Vec::new(), [0; 4096]);
    let header_end = loop {
        match socket.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => bytes.extend_from_slice(&chunk[..n]),
        }
        assert!(bytes.len() < 1 << 20, "fixture request too large");
        if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let header = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
    let length = header
        .lines()
        .find_map(|line| {
            let line = line.to_ascii_lowercase();
            line.strip_prefix("content-length:")
                .map(|v| v.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while bytes.len() < header_end + length {
        match socket.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => bytes.extend_from_slice(&chunk[..n]),
        }
    }
    let path = header
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let answer = {
        let mut state = state.lock().unwrap();
        let body = serde_json::from_slice(&bytes[header_end..header_end + length]);
        state.seen.push((path.clone(), body.unwrap_or(Value::Null)));
        match path.as_str() {
            "/props" => Answer::Json(state.props.clone()),
            "/v1/status" => Answer::Json(state.status.clone()),
            "/settings" => Answer::Json(state.settings.clone()),
            "/v1/responses" => {
                let posts = state.seen.iter().filter(|(p, _)| p == "/v1/responses");
                let posts = posts.count();
                if let Some(status) = state.statuses.remove(&posts) {
                    state.status = status;
                }
                state
                    .replies
                    .pop_front()
                    .map_or(Answer::Status(500), Answer::Stream)
            }
            _ => Answer::Status(404),
        }
    };
    let reply = match answer {
        Answer::Json(value) => {
            let json = serde_json::to_vec(&value).unwrap();
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                json.len()
            );
            let _ = socket.write_all(&[head.as_bytes(), &json].concat());
            return;
        }
        Answer::Status(status) => {
            let head = format!(
                "HTTP/1.1 {status} fixture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = socket.write_all(head.as_bytes());
            return;
        }
        Answer::Stream(reply) => reply,
    };
    // No length: the stream ends when the connection closes.
    let head = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
    if socket.write_all(head).is_err() {
        return;
    }
    match reply {
        Reply::Events(bytes) => {
            let _ = socket.write_all(&bytes);
        }
        Reply::Stall(bytes) => {
            if socket.write_all(&bytes).is_err() {
                return;
            }
            socket
                .set_read_timeout(Some(Duration::from_millis(20)))
                .unwrap();
            while !stop.load(Ordering::SeqCst) {
                match socket.read(&mut chunk) {
                    Ok(0) => return,
                    Ok(_) => {}
                    Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                    Err(_) => return,
                }
            }
        }
        Reply::Paced(frames, gap) => {
            for frame in frames {
                std::thread::sleep(gap);
                if stop.load(Ordering::SeqCst) || socket.write_all(&frame).is_err() {
                    return;
                }
            }
        }
    }
}
enum Answer {
    Json(Value),
    Status(u16),
    Stream(Reply),
}
