//! HTTP plumbing the S3 and Azure backends share: clients with the
//! timeouts object storage needs, and the encodings both signers use.

use std::fmt::Write as _;
use std::pin::Pin;
use std::task::{Context, Poll};

use super::{ByteStream, StorageError};

/// The longest a presigned URL lives: 7 days, S3's ceiling, used for Azure too.
pub(super) const MAX_PRESIGN_SECS: u64 = 604_800;

/// Default client timeouts; `with_http` on a backend replaces them.
pub(super) const DEFAULT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Waiting for a reply or the next body chunk; a total cap would fail big objects.
pub(super) const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// The slowest upload rate the default PUT timeout allows, in bytes per second.
const MIN_UPLOAD_RATE: u64 = 256 * 1024;

/// The HTTP clients. Only the defaults get a size-based upload timeout.
#[derive(Clone)]
pub(super) enum Http {
    /// reqwest's read timeout runs through an upload, so PUT uses a client without one.
    Default {
        http: reqwest::Client,
        upload: reqwest::Client,
        read_timeout: std::time::Duration,
    },
    /// From a backend's `with_http`: its own timeouts apply to every request.
    Custom(reqwest::Client),
}

impl Http {
    pub(super) fn defaults() -> Self {
        Self::with_read_timeout(DEFAULT_READ_TIMEOUT)
    }

    pub(super) fn with_read_timeout(read_timeout: std::time::Duration) -> Self {
        // A redirect could take signed requests to a host the config never named (#1780).
        let base = || {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
        };
        // A stalled endpoint must fail, not hang the request holding it (#2220).
        Self::Default {
            http: base()
                .read_timeout(read_timeout)
                .build()
                .expect("reqwest client builds"),
            upload: base().build().expect("reqwest client builds"),
            read_timeout,
        }
    }

    pub(super) fn request(
        &self,
        method: reqwest::Method,
        url: &str,
        len: u64,
    ) -> reqwest::RequestBuilder {
        match self {
            Self::Default {
                upload,
                read_timeout,
                ..
            } if method == reqwest::Method::PUT => upload
                .request(method, url)
                .timeout(upload_timeout(*read_timeout, len)),
            Self::Default { http, .. } | Self::Custom(http) => http.request(method, url),
        }
    }
}

/// The read timeout plus the time to send `len` bytes at [`MIN_UPLOAD_RATE`].
pub(super) fn upload_timeout(read_timeout: std::time::Duration, len: u64) -> std::time::Duration {
    read_timeout + std::time::Duration::from_millis(len.saturating_mul(1000) / MIN_UPLOAD_RATE)
}

/// A request body of exactly `len` bytes, sent as they arrive.
pub(super) fn stream_body(body: ByteStream, len: u64) -> reqwest::Body {
    reqwest::Body::wrap(StreamBody {
        body: std::sync::Mutex::new(body.exact(len)),
        len,
    })
}

/// reqwest wants a `Sync` body; the mutex gives that without locking,
/// as `poll_frame` has `&mut self`.
struct StreamBody {
    body: std::sync::Mutex<ByteStream>,
    len: u64,
}

impl http_body::Body for StreamBody {
    type Data = bytes::Bytes;
    type Error = StorageError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let body = self
            .get_mut()
            .body
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        futures_util::StreamExt::poll_next_unpin(body, cx)
            .map(|next| next.map(|chunk| chunk.map(http_body::Frame::data)))
    }

    fn size_hint(&self) -> http_body::SizeHint {
        http_body::SizeHint::with_exact(self.len)
    }
}

/// When `save_stream` splits an upload into parts, and how large they are.
#[derive(Clone, Copy, Debug)]
pub(super) struct Split {
    /// The most one request takes; larger bodies go in parts.
    pub max_single: u64,
    pub min_part: u64,
    pub max_parts: u64,
}

impl Split {
    /// The smallest part, at least `min_part`, that fits `len` in `max_parts`.
    pub(super) fn part_size(&self, len: u64) -> u64 {
        self.min_part.max(len.div_ceil(self.max_parts))
    }
}

/// A body of a known length cut into consecutive parts. Each part streams
/// as it arrives, so nothing is buffered beyond the chunk in hand.
pub(super) struct Parts {
    body: ByteStream,
    /// What the last part's final chunk carried past its end.
    rest: bytes::Bytes,
}

impl Parts {
    /// `body` must hold exactly `len` bytes; any other size ends in an error.
    pub(super) fn new(body: ByteStream, len: u64) -> Self {
        Self {
            body: body.exact(len),
            rest: bytes::Bytes::new(),
        }
    }

    /// The next `n` bytes. The remainder comes back on the receiver once
    /// all `n` are handed out; a body that fails first drops it.
    pub(super) fn take(self, n: u64) -> (ByteStream, tokio::sync::oneshot::Receiver<Parts>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let part = Part {
            src: Some(self),
            left: n,
            tx: Some(tx),
        };
        (ByteStream::new(part), rx)
    }

    /// Ok once the body is used up; an error if it holds more.
    pub(super) async fn finish(mut self) -> Result<(), StorageError> {
        use futures_util::StreamExt as _;
        let mut extra = !self.rest.is_empty();
        while !extra {
            match self.body.next().await {
                None => return Ok(()),
                Some(Err(e)) => return Err(e),
                Some(Ok(b)) => extra = !b.is_empty(),
            }
        }
        Err(StorageError::Io(
            "body is longer than the bytes declared".into(),
        ))
    }
}

/// [`Parts::take`].
struct Part {
    src: Option<Parts>,
    left: u64,
    tx: Option<tokio::sync::oneshot::Sender<Parts>>,
}

impl Part {
    /// Hand the rest back as soon as the last byte is out, so a client
    /// that stops polling at the declared length still returns it.
    fn give_back(&mut self) {
        if let (Some(src), Some(tx)) = (self.src.take(), self.tx.take()) {
            let _ = tx.send(src);
        }
    }
}

impl futures_util::Stream for Part {
    type Item = Result<bytes::Bytes, StorageError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.left == 0 {
            this.give_back();
            return Poll::Ready(None);
        }
        let Some(src) = this.src.as_mut() else {
            return Poll::Ready(None);
        };
        let mut chunk = if src.rest.is_empty() {
            let next = futures_util::StreamExt::poll_next_unpin(&mut src.body, cx);
            match std::task::ready!(next) {
                Some(Ok(b)) => b,
                Some(Err(e)) => {
                    this.src = None;
                    return Poll::Ready(Some(Err(e)));
                }
                // `exact` reports a short body before it ends.
                None => {
                    this.src = None;
                    return Poll::Ready(Some(Err(StorageError::Io(
                        "body ended before the bytes declared".into(),
                    ))));
                }
            }
        } else {
            std::mem::take(&mut src.rest)
        };
        if chunk.len() as u64 > this.left {
            // `left` is below `chunk.len()`, so it fits a usize.
            src.rest = chunk.split_off(this.left as usize);
        }
        this.left -= chunk.len() as u64;
        if this.left == 0 {
            this.give_back();
        }
        Poll::Ready(Some(Ok(chunk)))
    }
}

/// A failed request, with its causes: a streamed body's own error is
/// one of them.
pub(super) fn send_error(e: &reqwest::Error) -> StorageError {
    let mut msg = format!("http: {e}");
    let mut cause = std::error::Error::source(e);
    while let Some(c) = cause {
        let _ = write!(msg, ": {c}");
        cause = c.source();
    }
    StorageError::Io(msg)
}

/// A header value as the signers sign it: trimmed, inner runs of spaces
/// collapsed to one. `None` when empty; control characters are refused.
pub(super) fn canonical_header_value(v: Option<&str>) -> Result<Option<String>, StorageError> {
    let Some(v) = v else { return Ok(None) };
    if v.chars().any(|c| c.is_control() && c != '\t') {
        return Err(StorageError::Io(format!(
            "content type {v:?} has a control character"
        )));
    }
    let v = v
        .split([' ', '\t'])
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    Ok((!v.is_empty()).then_some(v))
}

/// Percent-encode a key for a URL path. `/` stays as it is, because
/// keys use it as a separator; everything outside the unreserved set
/// is escaped.
pub(super) fn encode_key(key: &str) -> String {
    percent_encode(key, true)
}

/// Like [`encode_key`], but it escapes `/` as well, for query values.
pub(super) fn encode_query(s: &str) -> String {
    percent_encode(s, false)
}

fn percent_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'~')
            || (keep_slash && b == b'/')
        {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

#[cfg(feature = "storage-azure")]
/// `(scheme://authority, path)` of an endpoint URL, the path without a
/// trailing `/`. A bare host is taken as https.
pub(super) fn split_endpoint(endpoint: &str) -> (String, String) {
    let (scheme, rest) = if let Some(r) = endpoint.strip_prefix("http://") {
        ("http", r)
    } else {
        (
            "https",
            endpoint.strip_prefix("https://").unwrap_or(endpoint),
        )
    };
    let rest = rest.trim_end_matches('/');
    match rest.find('/') {
        Some(i) => (format!("{scheme}://{}", &rest[..i]), rest[i..].to_owned()),
        None => (format!("{scheme}://{rest}"), String::new()),
    }
}

/// One-request HTTP servers for the backends' tests.
#[cfg(test)]
pub(super) mod testing {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    use bytes::Bytes;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use crate::storage::ByteStream;

    /// What one request carried.
    pub(in crate::storage) struct Served {
        pub head: String,
        /// Empty when counted instead.
        pub body: Vec<u8>,
        /// Body bytes that arrived before the client stopped.
        pub body_len: u64,
        /// The `content-length` the client declared.
        pub declared: u64,
    }

    /// Answer one request with `status`, `extra` header lines (each
    /// ending in `\r\n`) and `reply` as the body.
    pub(in crate::storage) async fn serve(
        status: u16,
        extra: String,
        reply: Vec<u8>,
    ) -> (String, tokio::task::JoinHandle<Served>) {
        serve_counted(status, extra, reply, None).await
    }

    /// [`serve`], counting the body into `count` instead of keeping it.
    pub(in crate::storage) async fn serve_counted(
        status: u16,
        extra: String,
        reply: Vec<u8>,
        count: Option<Arc<AtomicU64>>,
    ) -> (String, tokio::task::JoinHandle<Served>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let served = read_request(&mut sock, count.as_deref()).await;
            if served.whole() {
                write_reply(&mut sock, (status, extra, reply)).await;
            }
            served
        });
        (endpoint, task)
    }

    /// The answer to one request: status, extra header lines, body.
    pub(in crate::storage) type Reply = (u16, String, Vec<u8>);

    /// Answer every request with `respond(head)`, one connection each.
    /// Body bytes add up in `count` across requests, when given.
    pub(in crate::storage) async fn serve_many<F>(
        respond: F,
        count: Option<Arc<AtomicU64>>,
    ) -> (String, Arc<std::sync::Mutex<Vec<Served>>>)
    where
        F: Fn(&str) -> Reply + Send + Sync + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let respond = Arc::new(respond);
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let (respond, count, log) = (respond.clone(), count.clone(), log.clone());
                tokio::spawn(async move {
                    let mut sock = sock;
                    let served = read_request(&mut sock, count.as_deref()).await;
                    let reply = served.whole().then(|| respond(&served.head));
                    // Logged before the reply, so a finished call sees it.
                    log.lock().unwrap().push(served);
                    if let Some(reply) = reply {
                        write_reply(&mut sock, reply).await;
                    }
                });
            }
        });
        (endpoint, seen)
    }

    impl Served {
        /// Whether the head and the whole declared body arrived.
        fn whole(&self) -> bool {
            !self.head.is_empty() && self.body_len >= self.declared
        }
    }

    async fn write_reply(sock: &mut tokio::net::TcpStream, (status, extra, reply): Reply) {
        let head = format!(
            "HTTP/1.1 {status} X\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n",
            reply.len()
        );
        let _ = sock.write_all(head.as_bytes()).await;
        let _ = sock.write_all(&reply).await;
    }

    /// Read one request off `sock`, its body counted into `count` if given.
    async fn read_request(sock: &mut tokio::net::TcpStream, count: Option<&AtomicU64>) -> Served {
        let mut buf = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        // A client that gives up mid-body resets the connection.
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = sock.read(&mut chunk).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let end = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap_or(buf.len());
        let head = String::from_utf8_lossy(&buf[..end]).into_owned();
        let declared: u64 = head
            .to_ascii_lowercase()
            .lines()
            .find_map(|l| l.strip_prefix("content-length:").map(str::to_owned))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        let mut body = buf.split_off((end + 4).min(buf.len()));
        let mut body_len = body.len() as u64;
        if let Some(c) = count {
            c.fetch_add(body_len, Ordering::SeqCst);
            body.clear();
        }
        while body_len < declared {
            let n = sock.read(&mut chunk).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            body_len += n as u64;
            match count {
                Some(c) => {
                    c.fetch_add(n as u64, Ordering::SeqCst);
                }
                None => body.extend_from_slice(&chunk[..n]),
            }
        }
        Served {
            head,
            body,
            body_len,
            declared,
        }
    }

    /// `chunks` pieces of `size` bytes. Records in `ahead` the most bytes
    /// it handed out beyond what `received` says the server had.
    pub(in crate::storage) fn paced_body(
        chunks: usize,
        size: usize,
        received: Arc<AtomicU64>,
        ahead: Arc<AtomicU64>,
    ) -> ByteStream {
        let piece = Bytes::from(vec![b'z'; size]);
        ByteStream::new(futures_util::StreamExt::map(
            futures_util::stream::iter(0..chunks),
            move |i| {
                let lag = ((i * size) as u64).saturating_sub(received.load(Ordering::SeqCst));
                ahead.fetch_max(lag, Ordering::SeqCst);
                Ok(piece.clone())
            },
        ))
    }

    /// The head's value for `name`, or `None`.
    pub(in crate::storage) fn header(head: &str, name: &str) -> Option<String> {
        head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
        })
    }
}

#[cfg(all(test, feature = "storage-azure"))]
mod tests {
    use super::*;

    #[test]
    fn split_endpoint_separates_origin_and_path() {
        assert_eq!(
            split_endpoint("http://127.0.0.1:10000/devstoreaccount1/"),
            ("http://127.0.0.1:10000".into(), "/devstoreaccount1".into())
        );
        assert_eq!(
            split_endpoint("https://acct.blob.core.windows.net"),
            ("https://acct.blob.core.windows.net".into(), String::new())
        );
        assert_eq!(
            split_endpoint("cdn.example.com/files"),
            ("https://cdn.example.com".into(), "/files".into())
        );
    }
}
