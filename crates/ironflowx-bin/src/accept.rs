//! The host's accept loop: HTTP/1.1 and cleartext HTTP/2. A connection sends its first request's
//! head within 10 s, and on HTTP/1.1 each later one within 10 s of its first byte. A later head
//! that arrives with the request before it (as far as the host has read it when it answers that
//! one), and later HTTP/2 heads, count as idle, which has no limit.

use std::convert::Infallible;
use std::io;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};

use axum::Router;
use hyper::rt::{Sleep, Timer};
use hyper::service::{Service as _, service_fn};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use hyper_util::service::TowerToHyperService;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// Keeps connections serving while admitted requests drain, then closes their transports.
#[derive(Default)]
pub struct Connections {
    open: JoinSet<()>,
    requests: TaskTracker,
    graceful: CancellationToken,
    abort: CancellationToken,
}

impl Drop for Connections {
    fn drop(&mut self) {
        // Hyper also spawns HTTP/2 request tasks outside the connection's task.
        self.abort.cancel();
    }
}

impl Connections {
    /// Accepts until its caller drops this future and the listener.
    pub async fn accept(&mut self, listener: &TcpListener, router: &Router) -> Infallible {
        loop {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                Some(_) = self.open.join_next() => continue,
            };
            match accepted {
                Ok((stream, _)) => {
                    let _ = stream.set_nodelay(true);
                    self.open.spawn(connection(
                        stream,
                        router.clone(),
                        self.requests.clone(),
                        self.graceful.clone(),
                        self.abort.clone(),
                    ));
                }
                // The peer's fault, or too many open files: neither ends the loop.
                Err(e) => {
                    tracing::warn!("accept: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    /// The slot must already refuse new calls. Those calls can still use existing connections
    /// until the admitted requests finish. Then let Hyper flush and close the connections.
    pub async fn drain(&mut self, deadline: tokio::time::Instant) {
        self.requests.close();
        let graceful = async {
            self.requests.wait().await;
            self.graceful.cancel();
            while self.open.join_next().await.is_some() {}
        };
        if tokio::time::timeout_at(deadline, graceful).await.is_err() {
            self.abort.cancel();
            self.open.shutdown().await;
            // In HTTP/2 the connection can be gone before its handler future is dropped.
            self.requests.wait().await;
        }
    }
}

async fn connection<S>(
    stream: S,
    router: Router,
    requests: TaskTracker,
    graceful: CancellationToken,
    abort: CancellationToken,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let read = Arc::new(AtomicBool::new(false));
    let mut server = auto::Builder::new(TokioExecutor::new());
    server
        .http1()
        .timer(HeadTimer(read.clone()))
        .header_read_timeout(HEAD_TIMEOUT);
    let arrived = Arc::new(Notify::new());
    let (router, request) = (TowerToHyperService::new(router), arrived.clone());
    let service = service_fn(move |head| {
        request.notify_one();
        // Take the token before polling the door. After the tracker closes the slot has
        // already stopped admitting, so later requests can only refuse.
        let active = (!requests.is_closed()).then(|| requests.token());
        let abort = abort.clone();
        let answer = router.call(head);
        async move {
            let _active = active;
            tokio::select! {
                biased;
                () = abort.cancelled() => Err(io::Error::other("the host stopped")),
                response = answer => response.map_err(|never| match never {}),
            }
        }
    });
    let io = TokioIo::new(Noted { stream, read });
    let mut serving = pin!(server.serve_connection(io, service));
    // A connection that fails is its peer's to notice.
    tokio::select! {
        biased;
        _ = &mut serving => return,
        () = arrived.notified() => {}
        () = graceful.cancelled() => return,
        () = tokio::time::sleep(HEAD_TIMEOUT) => return,
    }
    tokio::select! {
        biased;
        _ = &mut serving => return,
        () = graceful.cancelled() => serving.as_mut().graceful_shutdown(),
    }
    let _ = serving.await;
}

/// Times an HTTP/1.1 head from its first byte. hyper's own timer runs from the end of the request
/// before, which would close an idle connection; the first head has the connection's deadline.
/// What was read of one before that end counts as idle; what is read after it is timed.
#[derive(Clone)]
struct HeadTimer(Arc<AtomicBool>);

impl Timer for HeadTimer {
    /// The clock its sleeps run on.
    fn now(&self) -> Instant {
        tokio::time::Instant::now().into_std()
    }

    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Sleep>> {
        self.0.store(false, Relaxed);
        Box::pin(HeadSleep {
            read: self.0.clone(),
            duration,
            sleep: None,
        })
    }

    fn sleep_until(&self, deadline: Instant) -> Pin<Box<dyn Sleep>> {
        self.sleep(deadline.saturating_duration_since(self.now()))
    }
}

/// Polled at each read of a head that is not whole yet.
struct HeadSleep {
    read: Arc<AtomicBool>,
    duration: Duration,
    sleep: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl Future for HeadSleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if !self.read.load(Relaxed) {
            return Poll::Pending;
        }
        let duration = self.duration;
        let sleep = self
            .sleep
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(duration)));
        sleep.as_mut().poll(cx)
    }
}

impl Sleep for HeadSleep {}

/// A connection that notes when bytes were read from it.
struct Noted<S> {
    stream: S,
    read: Arc<AtomicBool>,
}

impl<S: AsyncRead + Unpin> AsyncRead for Noted<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        ready!(Pin::new(&mut self.stream).poll_read(cx, buf))?;
        self.read.store(true, Relaxed);
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Noted<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use axum::routing::get;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream, duplex};
    use tokio::time::{Instant, timeout};

    use super::*;

    const WHOLE: &[u8] = b"GET /health HTTP/1.1\r\nhost: x\r\n\r\n";
    const HOUR: Duration = Duration::from_secs(3_600);

    fn connect() -> DuplexStream {
        let (ours, theirs) = duplex(64 * 1024);
        let slow = || tokio::time::sleep(Duration::from_secs(2));
        let router = Router::new()
            .route("/health", get(|| async {}))
            .route("/slow", get(slow));
        tokio::spawn(connection(
            theirs,
            router,
            TaskTracker::new(),
            CancellationToken::new(),
            CancellationToken::new(),
        ));
        ours
    }

    async fn answered(stream: &mut DuplexStream) {
        stream.write_all(WHOLE).await.unwrap();
        let mut answer = [0; 1024];
        let n = stream.read(&mut answer).await.unwrap();
        assert!(answer[..n].starts_with(b"HTTP/1.1 200 "));
    }

    /// How long from now until the host closes; `None` if it is open in an hour.
    async fn closed_after(stream: &mut DuplexStream) -> Option<Duration> {
        let from = Instant::now();
        let mut rest = Vec::new();
        let closed = timeout(HOUR, stream.read_to_end(&mut rest)).await;
        closed.ok().map(|_| from.elapsed())
    }

    /// 10 s, to the timer's millisecond.
    fn assert_10_s(after: Option<Duration>, name: &str) {
        let after = after.map(|t| t.as_millis());
        assert!(matches!(after, Some(10_000 | 10_001)), "{name}: {after:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn drain_ends_at_ten_seconds() {
        let (mut client, stream) = duplex(1024);
        let mut connections = Connections::default();
        let entered = Arc::new(Notify::new());
        let router = Router::new().route(
            "/health",
            get({
                let entered = entered.clone();
                move || async move {
                    entered.notify_one();
                    std::future::pending::<()>().await;
                }
            }),
        );
        connections.open.spawn(connection(
            stream,
            router,
            connections.requests.clone(),
            connections.graceful.clone(),
            connections.abort.clone(),
        ));
        client.write_all(WHOLE).await.unwrap();
        entered.notified().await;
        let deadline = crate::drain_deadline(Instant::now());
        let ((), after) = tokio::join!(connections.drain(deadline), closed_after(&mut client));
        assert_10_s(after, "drain");
    }

    #[tokio::test(start_paused = true)]
    async fn a_connection_has_10_s_for_its_first_head() {
        for first in [
            "",
            "GET /health HTTP/1.1\r\nhost",
            "PRI * HTT",
            "PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n",
        ] {
            let mut stream = connect();
            stream.write_all(first.as_bytes()).await.unwrap();
            assert_10_s(closed_after(&mut stream).await, first);
        }
        // Each byte in time, the whole head not.
        let mut stream = connect();
        for byte in &WHOLE[..9] {
            stream.write_all(&[*byte]).await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let after = closed_after(&mut stream).await.unwrap();
        assert_10_s(Some(after + Duration::from_secs(9)), "a byte a second");
    }

    #[tokio::test(start_paused = true)]
    async fn a_later_head_has_10_s_from_its_first_byte() {
        for idle in [Duration::ZERO, Duration::from_secs(9), HOUR] {
            let mut stream = connect();
            for _ in 0..2 {
                answered(&mut stream).await;
                tokio::time::sleep(idle).await;
            }
            stream.write_all(b"G").await.unwrap();
            tokio::time::sleep(Duration::from_secs(9)).await;
            stream.write_all(b"E").await.unwrap();
            let after = closed_after(&mut stream).await.unwrap();
            assert_10_s(Some(after + Duration::from_secs(9)), &format!("{idle:?}"));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_head_that_arrives_with_the_request_before_it_counts_as_idle() {
        let mut stream = connect();
        stream.write_all(WHOLE).await.unwrap();
        stream.write_all(b"GET /he").await.unwrap();
        assert_eq!(closed_after(&mut stream).await, None);
        stream.write_all(b"a").await.unwrap();
        assert_10_s(closed_after(&mut stream).await, "more of it");
    }

    const SLOW: &[u8] = b"GET /slow HTTP/1.1\r\nhost: x\r\n\r\n";

    #[tokio::test(start_paused = true)]
    async fn a_head_read_before_the_request_before_it_is_answered_counts_as_idle() {
        let mut stream = connect();
        stream.write_all(SLOW).await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        stream.write_all(b"GET /he").await.unwrap();
        assert_eq!(closed_after(&mut stream).await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn the_rest_of_such_a_head_is_timed_from_that_answer() {
        let mut stream = connect();
        stream
            .write_all(&[SLOW, b"GET /he"].concat())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        stream.write_all(b"a").await.unwrap();
        // The answer comes a second from now.
        let after = closed_after(&mut stream).await.unwrap();
        assert_10_s(Some(after - Duration::from_secs(1)), "its rest");
    }

    #[tokio::test(start_paused = true)]
    async fn a_later_http_2_head_counts_as_idle() {
        let frame = |kind: u8, flags: u8, stream: u8, payload: &[u8]| {
            let len = u8::try_from(payload.len()).unwrap();
            [&[0, 0, len, kind, flags, 0, 0, 0, stream], payload].concat()
        };
        let (headers, end_stream, end_headers) = (1, 1, 4);
        // GET, http, /health, x: HPACK, with no Huffman code.
        let get = b"\x82\x86\x04\x07/health\x01\x01x";
        let settings = frame(4, 0, 0, b"");
        let whole = frame(headers, end_stream | end_headers, 1, get);
        let unended = frame(headers, end_stream, 3, get);
        for part in [&unended[..], &unended[..12]] {
            let mut stream = connect();
            let preface = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
            let sent = [&preface[..], &settings, &whole].concat();
            stream.write_all(&sent).await.unwrap();
            // The answer's HEADERS, on stream 1.
            let mut answer = Vec::new();
            while !answer.windows(5).any(|w| w == [0, 0, 0, 1, 0x88]) {
                assert_ne!(stream.read_buf(&mut answer).await.unwrap(), 0);
            }
            stream.write_all(part).await.unwrap();
            assert_eq!(closed_after(&mut stream).await, None);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_connection_has_no_limit() {
        let mut stream = connect();
        answered(&mut stream).await;
        assert_eq!(closed_after(&mut stream).await, None);
        answered(&mut stream).await;
    }
}
