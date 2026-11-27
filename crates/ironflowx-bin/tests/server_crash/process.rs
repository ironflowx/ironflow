//! Child ownership and the two public log streams. Only the private socket receives keys.

use std::fs;
use std::io::{BufRead as _, BufReader, Read, Write as _};
use std::net::SocketAddr;
use std::os::fd::OwnedFd;
use std::os::unix::{net::UnixStream, process::ExitStatusExt as _};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ironflowx_core::{FaultPoint, KeyId, Secret};
use ironflowx_operations::key_id;
use ironflowx_proto::Client;
use rustix::process::{Pid, Signal, kill_process};
use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::leak;

const WAIT: Duration = Duration::from_secs(15);
const LOST_SECRET: &str = "an earlier start minted a key nobody received; this open revoked it";

pub struct Told {
    pub id: KeyId,
    pub key: Secret,
}

pub struct Output {
    pub keys: Vec<Told>,
    pub stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl Output {
    pub fn check_secrets<'a>(&'a self, additional: impl IntoIterator<Item = &'a Secret>) {
        let keys: Vec<_> = self
            .keys
            .iter()
            .map(|t| t.key.expose())
            .chain(additional.into_iter().map(Secret::expose))
            .collect();
        for (name, bytes) in [("stdout", &self.stdout), ("stderr", &self.stderr)] {
            assert_eq!(
                leak::leak(&leak::latin1(bytes), keys.iter().copied()),
                None,
                "{name}"
            );
        }
    }

    pub fn lost_secret(&self) -> bool {
        self.stderr.split(|&b| b == b'\n').any(|line| {
            serde_json::from_slice::<Value>(line)
                .is_ok_and(|v| v["fields"]["message"] == LOST_SECRET)
        })
    }
}

pub struct Process {
    child: Child,
    socket: UnixStream,
    stdout: Capture,
    stderr: Capture,
    addr: Option<SocketAddr>,
}

struct Capture {
    lines: Receiver<Vec<u8>>,
    reader: Option<JoinHandle<Vec<u8>>>,
}

impl Capture {
    fn new(pipe: impl Read + Send + 'static) -> Self {
        let (send, lines) = channel();
        let reader = thread::spawn(move || {
            let mut pipe = BufReader::new(pipe);
            let mut all = Vec::new();
            let mut line = Vec::new();
            while pipe
                .read_until(b'\n', &mut line)
                .expect("read process output")
                != 0
            {
                all.extend_from_slice(&line);
                let _ = send.send(std::mem::take(&mut line));
            }
            all
        });
        Self {
            lines,
            reader: Some(reader),
        }
    }

    fn until<T>(&self, description: &str, mut pick: impl FnMut(&[u8]) -> Option<T>) -> T {
        let deadline = Instant::now() + WAIT;
        loop {
            let line = self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|e| panic!("waiting for {description}: {e}"));
            if let Some(value) = pick(&line) {
                return value;
            }
        }
    }

    fn finish(&mut self) -> Vec<u8> {
        self.reader
            .take()
            .unwrap()
            .join()
            .expect("collect child output")
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // After try_wait reaps the child, kill is a no-op.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Process {
    pub fn command() -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ironflowx"));
        // No caller-provided settings, authentication, logging, or proxies reach the child.
        command.env_clear();
        command
    }

    pub fn serve(root: &Path, seed_file: Option<&Path>, point: Option<(FaultPoint, u64)>) -> Self {
        let mut command = Self::command();
        command.args(["serve", "--listen", "127.0.0.1:0", "--log-format", "json"]);
        command.arg("--data-dir").arg(root);
        if let Some(seed_file) = seed_file {
            command.arg("--seed-file").arg(seed_file);
        }
        Self::start(command, point)
    }

    pub fn recover(root: &Path, point: Option<FaultPoint>) -> Self {
        let mut command = Self::command();
        command.args(["recover-key", "--data-dir"]).arg(root);
        Self::start(command, point.map(|p| (p, 1)))
    }

    pub fn start(mut command: Command, point: Option<(FaultPoint, u64)>) -> Self {
        let (socket, theirs) = UnixStream::pair().unwrap();
        socket.set_read_timeout(Some(WAIT)).unwrap();
        socket.set_write_timeout(Some(WAIT)).unwrap();
        let config = point.map_or_else(|| "- 1".to_owned(), |(p, n)| format!("{} {n}", p.name()));
        let mut child = command
            .env("IFX_SERVER_CRASH", config)
            .stdin(OwnedFd::from(theirs))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the real ironflowx");
        let stdout = Capture::new(child.stdout.take().unwrap());
        let stderr = Capture::new(child.stderr.take().unwrap());
        Self {
            child,
            socket,
            stdout,
            stderr,
            addr: None,
        }
    }

    pub fn at(&self, point: FaultPoint) {
        let expected = format!("at {}\n", point.name());
        self.stdout.until(point.name(), |line| {
            (line == expected.as_bytes()).then_some(())
        });
    }

    pub fn release(&self) {
        (&self.socket)
            .write_all(&[1])
            .expect("release the parked writer");
    }

    pub fn log(&self, message: &str) -> Value {
        self.stderr.until(message, |line| {
            let row = serde_json::from_slice::<Value>(line).ok()?;
            (row["fields"]["message"] == message).then(|| row["fields"].clone())
        })
    }

    pub fn url(&mut self) -> String {
        let addr = match self.addr {
            Some(addr) => addr,
            None => {
                let fields = self.log("listening");
                let addr: SocketAddr = fields["url"]
                    .as_str()
                    .expect("listening URL")
                    .strip_prefix("http://")
                    .expect("HTTP address")
                    .parse()
                    .expect("listening socket address");
                assert!(addr.ip().is_loopback());
                self.addr = Some(addr);
                addr
            }
        };
        format!("http://{addr}")
    }

    pub async fn ready(&mut self) -> Client {
        let url = self.url();
        let addr = self.addr.unwrap();
        tokio::time::timeout(WAIT, async {
            loop {
                let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
                stream
                    .write_all(
                        b"GET /ready HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .unwrap();
                let mut response = Vec::new();
                stream.read_to_end(&mut response).await.unwrap();
                if response.starts_with(b"HTTP/1.1 200 ") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the restarted server becomes ready");
        Client::new(&url).unwrap().with_timeout(WAIT)
    }

    /// Hyper's interim reply proves this connection reached the door. Keep its body unsent so
    /// the drain cannot finish as soon as the writer's other connection answers.
    pub async fn hold_request_body(&self) -> tokio::net::TcpStream {
        tokio::time::timeout(WAIT, async {
            let mut stream = tokio::net::TcpStream::connect(self.addr.unwrap())
                .await
                .unwrap();
            stream
                .write_all(
                    b"POST /ironflowx.v1.EventService/Emit HTTP/1.1\r\nHost: fixture\r\n\
                  Content-Type: application/json\r\nContent-Length: 2\r\n\
                  Expect: 100-continue\r\n\r\n",
                )
                .await
                .unwrap();
            let mut response = [0; 25];
            stream.read_exact(&mut response).await.unwrap();
            assert!(
                &response == b"HTTP/1.1 100 Continue\r\n\r\n",
                "the door is reading a body"
            );
            stream
        })
        .await
        .expect("the held request reached the door")
    }

    pub fn signal(&self, signal: Signal) {
        kill_process(Pid::from_child(&self.child), signal).unwrap();
    }

    pub fn stop(self) -> Output {
        self.signal(Signal::TERM);
        self.success()
    }

    pub fn success(self) -> Output {
        let (status, output) = self.finish();
        assert_eq!(status.code(), Some(0), "the child exits successfully");
        output
    }

    pub fn kill(mut self) -> Output {
        self.child.kill().unwrap();
        let (status, output) = self.finish();
        assert_eq!(status.signal(), Some(9), "the child was killed by SIGKILL");
        output
    }

    fn finish(mut self) -> (ExitStatus, Output) {
        let deadline = Instant::now() + WAIT;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "the child did not exit");
            thread::sleep(Duration::from_millis(5));
        };

        let mut told = String::new();
        self.socket
            .read_to_string(&mut told)
            .expect("the private socket reads");
        let keys = told
            .lines()
            .filter_map(|line| {
                let [_, id, key] = line.split(' ').collect::<Vec<_>>()[..] else {
                    panic!("malformed key notification");
                };
                (key != "-").then(|| {
                    let id = id.parse().expect("a key ID");
                    let key = Secret::new(key.to_owned());
                    assert_eq!(key_id(&key), Some(id), "the private key matches its ID");
                    Told { id, key }
                })
            })
            .collect();
        let output = Output {
            keys,
            stdout: self.stdout.finish(),
            stderr: self.stderr.finish(),
        };
        output.check_secrets([]);
        (status, output)
    }
}

pub fn write_seed(path: &Path, seed: &Secret) {
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = fs::File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    writeln!(file, "{}", seed.expose()).unwrap();
}
