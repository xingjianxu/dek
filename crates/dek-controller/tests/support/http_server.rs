use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

#[derive(Default)]
struct ServerState {
    responses: BTreeMap<String, (u16, String)>,
    requests: BTreeMap<String, usize>,
}

pub struct Server {
    port: u16,
    state: Arc<Mutex<ServerState>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Server {
    pub fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let state = Arc::new(Mutex::new(ServerState::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::clone(&state);
        let stop_signal = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !stop_signal.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("fixture connection: {error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split_ascii_whitespace().nth(1).unwrap().to_owned();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
                let (status, body) = {
                    let mut state = shared.lock().unwrap();
                    *state.requests.entry(path.clone()).or_default() += 1;
                    state
                        .responses
                        .get(&path)
                        .cloned()
                        .unwrap_or((404, String::new()))
                };
                write!(
                    stream,
                    "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                ).unwrap();
            }
        });
        Self {
            port,
            state,
            stop,
            worker: Some(worker),
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    pub fn respond(&self, path: &str, status: u16, body: &str) {
        self.state
            .lock()
            .unwrap()
            .responses
            .insert(path.into(), (status, body.into()));
    }

    pub fn count(&self, path: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .requests
            .get(path)
            .copied()
            .unwrap_or(0)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}
