//! テスト専用の極小 HTTP サーバ。
//!
//! STT / 整形クライアントを**実 API を叩かずに**検証するために使う。
//! モック用クレートを足さずに済ませたいのと、リトライ・タイムアウト・
//! ヘッダの検証は「本物の reqwest が本物のソケットに喋る」形でないと
//! 意味がないため、127.0.0.1 に使い捨てのリスナを立てる。
//!
//! 応答はキューで与え、先頭から 1 リクエストにつき 1 つ消費する。
//! すべての応答に `Connection: close` を付けるので、キープアライブで
//! 2 回目のリクエストが同じ接続に乗ることはない (= 1 接続 1 応答が保証される)。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// 返す応答の定義。
#[derive(Debug, Clone)]
pub struct CannedResponse {
    pub status: u16,
    pub body: String,
    /// 応答を書き始めるまでの待ち時間 (タイムアウト検証用)。
    pub delay: Duration,
}

impl CannedResponse {
    pub fn ok(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            body: body.into(),
            delay: Duration::ZERO,
        }
    }

    pub fn status(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            delay: Duration::ZERO,
        }
    }

    pub fn slow(delay: Duration) -> Self {
        Self {
            status: 200,
            body: "{}".to_string(),
            delay,
        }
    }
}

/// 受け取ったリクエストの記録。
#[derive(Debug, Clone, Default)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RecordedRequest {
    /// ヘッダ値を大文字小文字を無視して引く。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// ボディを UTF-8 として読む (multipart でも検査に使える)。
    pub fn body_lossy(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

pub struct TestServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl TestServer {
    /// 応答キューを与えてサーバを起動する。キューを撃ち尽くしたら終了する。
    pub fn start(responses: Vec<CannedResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("テスト用ポートを確保できない");
        let addr = listener.local_addr().expect("ローカルアドレスを取れない");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&requests);

        thread::spawn(move || {
            for response in responses {
                let Ok((stream, _)) = listener.accept() else {
                    break;
                };
                // 接続ごとの失敗はテスト対象ではないので握り潰す
                // (タイムアウト検証ではクライアントが先に切る)。
                let _ = serve_one(stream, &response, &sink);
            }
        });

        Self { addr, requests }
    }

    /// `http://127.0.0.1:<port>` 形式のベース URL。
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// これまでに受け取ったリクエスト。
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests
            .lock()
            .map(|r| r.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    pub fn request_count(&self) -> usize {
        self.requests().len()
    }
}

fn serve_one(
    stream: std::net::TcpStream,
    response: &CannedResponse,
    sink: &Arc<Mutex<Vec<RecordedRequest>>>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut record = RecordedRequest::default();

    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    record.method = parts.next().unwrap_or_default().to_string();
    record.path = parts.next().unwrap_or_default().to_string();

    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let (name, value) = (name.trim().to_string(), value.trim().to_string());
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.parse().unwrap_or(0);
            }
            record.headers.push((name, value));
        }
    }

    if content_length > 0 {
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body)?;
        record.body = body;
    }

    if let Ok(mut guard) = sink.lock() {
        guard.push(record);
    }

    if !response.delay.is_zero() {
        thread::sleep(response.delay);
    }

    let mut out = stream;
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        reason_phrase(response.status),
        response.body.len()
    );
    out.write_all(head.as_bytes())?;
    out.write_all(response.body.as_bytes())?;
    out.flush()
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}
