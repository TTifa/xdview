//! 最小 HTTP/1.1 客户端（仅 plaintext）。
//!
//! ONVIF 云台控制只需要「POST 一段 SOAP，读回一段 XML」，为它引入 `reqwest` + `rustls`
//! 会把依赖树从 1 个 crate 撑到 100+，并带来 C 编译器要求。局域网 IPC 的 ONVIF 服务
//! 绝大多数是 `http://`，因此这里手写一个够用的客户端。
//!
//! 不支持：HTTPS（`https://` 会返回明确错误）、代理、HTTP/2、keep-alive 复用。

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Url {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Url {
    pub fn parse(raw: &str) -> Result<Url, String> {
        let raw = raw.trim();
        if raw.contains(['\r', '\n']) {
            return Err("URL 包含非法换行".into());
        }
        let (scheme, rest) = raw.split_once("://").ok_or("URL 缺少 scheme")?;
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "http" {
            return Err(format!(
                "只支持 http://（收到 {scheme}://）。ONVIF 若为 HTTPS，请在相机侧关闭 TLS 或改用 http 端口"
            ));
        }
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], rest[i..].to_string()),
            None => (rest, "/".to_string()),
        };
        if authority.is_empty() {
            return Err("URL 缺少主机".into());
        }
        if authority.contains('@') {
            return Err("ONVIF 地址不能内嵌账号，请使用用户名和密码字段".into());
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
                let port = p.parse::<u16>().map_err(|_| format!("端口非法: {p}"))?;
                (h.to_string(), port)
            }
            _ => (authority.to_string(), 80),
        };
        Ok(Url {
            scheme,
            host,
            port,
            path,
        })
    }

    pub fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub body: Vec<u8>,
    /// 响应头（原样保留大小写，查找时忽略大小写）。
    ///
    /// Digest 鉴权要靠它读 `WWW-Authenticate` 拿 realm/nonce，
    /// 因此不做丢弃。
    pub headers: Vec<(String, String)>,
}

impl Response {
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// 按名字取响应头（忽略大小写），同名多个时返回第一个。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

const MAX_BODY: usize = 8 * 1024 * 1024;
const MAX_HEAD: usize = 64 * 1024;

/// 明文 HTTP POST；timeout 是 DNS、连接、发送和完整响应的共同期限。
/// 只接受 close-delimited、单个 Content-Length 或 chunked，不接受歧义 framing。
pub fn post(
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    timeout: Duration,
) -> Result<Response, String> {
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .ok_or("HTTP 超时时间无效")?;
    let u = Url::parse(url)?;
    if body.len() > MAX_BODY {
        return Err("HTTP 请求体超过 8 MiB".into());
    }
    let addr = u.authority();
    let addresses = resolve(&addr, deadline)?;
    let mut connected = None;
    for address in addresses {
        if let Ok(stream) = TcpStream::connect_timeout(&address, remaining(deadline)?) {
            connected = Some(stream);
            break;
        }
    }
    let mut stream = connected.ok_or_else(|| format!("连接 {addr} 失败，请检查设备地址和网络"))?;
    let mut req = format!("POST {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nUser-Agent: xdview/0.1\r\nContent-Length: {}\r\n", u.path, addr, body.len());
    for (k, v) in headers {
        if k.is_empty()
            || !k.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
            || v.contains(['\r', '\n'])
        {
            return Err("HTTP 请求头包含非法字符".into());
        }
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    for data in [req.as_bytes(), body] {
        let mut offset = 0;
        while offset < data.len() {
            stream
                .set_write_timeout(Some(remaining(deadline)?))
                .map_err(|e| format!("设置写超时失败：{e}"))?;
            match stream.write(&data[offset..]) {
                Ok(0) => return Err("HTTP 发送中断".into()),
                Ok(n) => offset += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(format!("HTTP 发送失败：{e}")),
            }
        }
    }
    let mut reader = std::io::BufReader::new(DeadlineStream { stream, deadline });
    read_response(&mut reader)
}

fn remaining(deadline: std::time::Instant) -> Result<Duration, String> {
    deadline
        .checked_duration_since(std::time::Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| "HTTP 请求超过总期限，请检查设备响应或网络".into())
}

struct DeadlineStream {
    stream: TcpStream,
    deadline: std::time::Instant,
}

impl Read for DeadlineStream {
    fn read(&mut self, data: &mut [u8]) -> std::io::Result<usize> {
        let timeout = remaining(self.deadline)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::TimedOut, e))?;
        self.stream.set_read_timeout(Some(timeout))?;
        self.stream.read(data)
    }
}

type DnsRequest = (
    String,
    std::time::Instant,
    std::sync::mpsc::SyncSender<Result<Vec<std::net::SocketAddr>, String>>,
);

/// 标准库 DNS 不支持取消。使用唯一、有界的解析线程，超时调用方可返回且不会无限创建线程。
fn resolve(addr: &str, deadline: std::time::Instant) -> Result<Vec<std::net::SocketAddr>, String> {
    if let Ok(ip) = addr.parse::<std::net::SocketAddr>() {
        return Ok(vec![ip]);
    }
    static DNS: std::sync::OnceLock<Result<std::sync::mpsc::SyncSender<DnsRequest>, String>> =
        std::sync::OnceLock::new();
    let resolver = DNS
        .get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::sync_channel::<DnsRequest>(1);
            std::thread::Builder::new()
                .name("xdview-dns".into())
                .spawn(move || {
                    while let Ok((addr, deadline, reply)) = rx.recv() {
                        if remaining(deadline).is_err() {
                            continue;
                        }
                        let result = addr
                            .to_socket_addrs()
                            .map(|a| a.collect::<Vec<_>>())
                            .map_err(|e| format!("解析设备地址失败：{e}"));
                        let _ = reply.send(result);
                    }
                })
                .map_err(|e| format!("无法启动地址解析线程：{e}"))?;
            Ok(tx)
        })
        .as_ref()
        .map_err(Clone::clone)?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    resolver
        .try_send((addr.to_string(), deadline, tx))
        .map_err(|_| "地址解析繁忙，请稍后重试")?;
    let addresses = rx
        .recv_timeout(remaining(deadline)?)
        .map_err(|_| "设备地址解析超时，请检查 DNS 或使用 IP 地址")??;
    if addresses.is_empty() {
        return Err("设备地址没有可用 IP".into());
    }
    Ok(addresses)
}

fn read_line(reader: &mut impl std::io::BufRead, budget: &mut usize) -> Result<Vec<u8>, String> {
    let mut line = Vec::new();
    loop {
        let buffer = reader
            .fill_buf()
            .map_err(|e| format!("读取 HTTP 响应失败：{e}"))?;
        if buffer.is_empty() {
            return Err("HTTP 响应截断，缺少完整行".into());
        }
        let count = buffer
            .iter()
            .position(|b| *b == b'\n')
            .map_or(buffer.len(), |i| i + 1);
        if count > *budget {
            return Err("HTTP 头部或分块信息过大".into());
        }
        let complete = buffer[count - 1] == b'\n';
        line.extend_from_slice(&buffer[..count]);
        *budget -= count;
        reader.consume(count);
        if complete {
            if !line.ends_with(b"\r\n") {
                return Err("HTTP 行缺少 CRLF".into());
            }
            line.truncate(line.len() - 2);
            return Ok(line);
        }
    }
}

fn read_response(reader: &mut impl std::io::BufRead) -> Result<Response, String> {
    let mut head_budget = MAX_HEAD;
    // 兼容已记录的设备回显：只允许在有界头部区域内跳到真正的状态行。
    let status_line = loop {
        let line = read_line(reader, &mut head_budget)?;
        let mut candidate = line.clone();
        candidate.extend_from_slice(b"\r\n");
        if let Some(start) = find_status_line_start(&candidate) {
            break String::from_utf8(line[start..].to_vec())
                .map_err(|_| "HTTP 状态行不是 UTF-8")?;
        }
    };
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
        return Err("不支持的 HTTP 版本".into());
    }
    let status = parts
        .next()
        .ok_or("HTTP 缺少状态码")?
        .parse::<u16>()
        .map_err(|_| "HTTP 状态码非法")?;
    if !(200..=599).contains(&status) {
        return Err("不支持的 HTTP 临时响应或状态码".into());
    }
    let reason = parts.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    let mut length = None;
    let mut chunked = false;
    loop {
        let line = read_line(reader, &mut head_budget)?;
        if line.is_empty() {
            break;
        }
        let line = std::str::from_utf8(&line).map_err(|_| "HTTP 头部不是 UTF-8")?;
        let (key, value) = line.split_once(':').ok_or("HTTP 头部缺少冒号")?;
        let (key, value) = (key.trim(), value.trim());
        if key.eq_ignore_ascii_case("Content-Length") {
            if length.is_some() {
                return Err("HTTP Content-Length 重复".into());
            }
            let size = value
                .parse::<usize>()
                .map_err(|_| "HTTP Content-Length 非法")?;
            if size > MAX_BODY {
                return Err("HTTP 响应体超过 8 MiB".into());
            }
            length = Some(size);
        }
        if key.eq_ignore_ascii_case("Transfer-Encoding") {
            if chunked || !value.eq_ignore_ascii_case("chunked") {
                return Err("不支持或重复的 HTTP Transfer-Encoding".into());
            }
            chunked = true;
        }
        headers.push((key.to_string(), value.to_string()));
    }
    if chunked && length.is_some() {
        return Err("HTTP 响应含歧义的长度声明".into());
    }
    let mut body = Vec::new();
    if matches!(status, 204 | 304) {
        // 按 HTTP 语义，这两类响应没有消息体。
    } else if chunked {
        let mut metadata_budget = MAX_HEAD;
        loop {
            let line = read_line(reader, &mut metadata_budget)?;
            let size_text = std::str::from_utf8(&line)
                .map_err(|_| "分块长度不是 ASCII")?
                .split(';')
                .next()
                .unwrap_or("");
            let size = usize::from_str_radix(size_text.trim(), 16).map_err(|_| "分块长度非法")?;
            if size == 0 {
                loop {
                    let trailer = read_line(reader, &mut metadata_budget)?;
                    if trailer.is_empty() {
                        break;
                    }
                    if !trailer.contains(&b':') {
                        return Err("HTTP trailer 非法".into());
                    }
                }
                break;
            }
            let new_len = body
                .len()
                .checked_add(size)
                .filter(|n| *n <= MAX_BODY)
                .ok_or("HTTP 响应体超过 8 MiB")?;
            let old_len = body.len();
            body.resize(new_len, 0);
            reader
                .read_exact(&mut body[old_len..])
                .map_err(|e| format!("HTTP 分块截断或超时：{e}"))?;
            let mut ending = [0; 2];
            reader
                .read_exact(&mut ending)
                .map_err(|e| format!("HTTP 分块缺少结束标记：{e}"))?;
            if ending != *b"\r\n" {
                return Err("HTTP 分块缺少 CRLF".into());
            }
        }
    } else if let Some(length) = length {
        body.resize(length, 0);
        reader
            .read_exact(&mut body)
            .map_err(|e| format!("HTTP 响应体截断或超时：{e}"))?;
    } else {
        let mut buffer = [0; 8192];
        loop {
            let n = reader.read(&mut buffer).map_err(|e| {
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) {
                    "HTTP 响应未声明长度且未关闭连接，无法确认消息完整；请检查设备固件或 ONVIF 服务"
                        .to_string()
                } else {
                    format!("读取 HTTP 响应体失败：{e}")
                }
            })?;
            if n == 0 {
                break;
            }
            if body.len() + n > MAX_BODY {
                return Err("HTTP 响应体超过 8 MiB".into());
            }
            body.extend_from_slice(&buffer[..n]);
        }
    }
    Ok(Response {
        status,
        reason,
        body,
        headers,
    })
}

/// 在有界的头部区域识别状态行，兼容设备在真正响应前回显请求。
fn find_status_line_start(raw: &[u8]) -> Option<usize> {
    let pat = b"HTTP/1.";
    let mut idx = 0;
    while let Some(pos) = find_subslice(&raw[idx..], pat) {
        let abs = idx + pos;
        if let Some(t) = raw.get(abs + 7..abs + 12) {
            if matches!(t[0], b'0' | b'1')
                && t[1] == b' '
                && t[2..].iter().all(|b| b.is_ascii_digit())
            {
                return Some(abs);
            }
        }
        idx = abs + 1;
    }
    None
}

#[cfg(test)]
fn parse_response(raw: &[u8]) -> Result<Response, String> {
    read_response(&mut std::io::BufReader::new(std::io::Cursor::new(raw)))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn serve(
        work: impl FnOnce(TcpStream) + Send + 'static,
    ) -> (String, std::thread::JoinHandle<()>) {
        use std::io::BufRead;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/test", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut reader = std::io::BufReader::new(&mut stream);
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
            }
            work(stream);
        });
        (url, handle)
    }

    #[test]
    fn socket_reads_split_body_without_waiting_for_connection_close() {
        let (url, worker) = serve(|mut stream| {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhe")
                .unwrap();
            std::thread::sleep(Duration::from_millis(20));
            stream.write_all(b"llo").unwrap();
            std::thread::sleep(Duration::from_millis(350));
        });
        let response = post(&url, &[], b"", Duration::from_millis(250)).unwrap();
        assert_eq!(response.body, b"hello");
        worker.join().unwrap();
    }

    #[test]
    fn socket_requires_eof_for_response_without_length() {
        for keep_open in [false, true] {
            let (url, worker) = serve(move |mut stream| {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/soap+xml\r\n\r\n<Envelope>",
                    )
                    .unwrap();
                std::thread::sleep(Duration::from_millis(10));
                stream.write_all(b"<Body/></Envelope>").unwrap();
                if keep_open {
                    std::thread::sleep(Duration::from_millis(300));
                }
            });
            let result = post(&url, &[], b"", Duration::from_millis(150));
            if keep_open {
                assert!(result.unwrap_err().contains("未关闭连接"));
            } else {
                assert_eq!(result.unwrap().body, b"<Envelope><Body/></Envelope>");
            }
            worker.join().unwrap();
        }
    }

    #[test]
    fn socket_rejects_truncation_and_slow_drip_within_total_deadline() {
        for response in [
            &b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nabc"[..],
            &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n9\r\nabc"[..],
        ] {
            let (url, worker) = serve(move |mut stream| {
                stream.write_all(response).unwrap();
            });
            assert!(post(&url, &[], b"", Duration::from_secs(1)).is_err());
            worker.join().unwrap();
        }
        let (url, worker) = serve(|mut stream| {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n")
                .unwrap();
            for _ in 0..20 {
                if stream.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let started = std::time::Instant::now();
        assert!(post(&url, &[], b"", Duration::from_millis(100)).is_err());
        assert!(started.elapsed() < Duration::from_millis(400));
        worker.join().unwrap();
    }

    #[test]
    fn rejects_truncated_response_bodies() {
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n<ok/>").is_err());
        assert!(
            parse_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n10\r\nabc")
                .is_err()
        );
        assert!(parse_response(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n"
        )
        .is_err());
    }

    #[test]
    fn parse_plain_url() {
        let u = Url::parse("http://192.168.1.10/onvif/ptz_service").unwrap();
        assert_eq!(u.host, "192.168.1.10");
        assert_eq!(u.port, 80);
        assert_eq!(u.path, "/onvif/ptz_service");
    }

    #[test]
    fn parse_url_with_port_and_no_path() {
        let u = Url::parse("http://cam.local:8080").unwrap();
        assert_eq!(u.host, "cam.local");
        assert_eq!(u.port, 8080);
        assert_eq!(u.path, "/");
    }

    #[test]
    fn https_rejected_with_reason() {
        let err = Url::parse("https://x/y").unwrap_err();
        assert!(err.contains("https"));
    }

    #[test]
    fn parse_response_with_content_length() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\n\r\n<hello/>";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body_text(), "<hello/>");
    }

    #[test]
    fn parse_chunked_response() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.body_text(), "hello world");
    }

    #[test]
    fn exposes_digest_challenge_header() {
        let raw = b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"IP Camera\", qop=\"auth\", nonce=\"abc123\"\r\nContent-Length: 0\r\n\r\n";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.status, 401);
        let chal = r.header("WWW-Authenticate").expect("应能取到质询头");
        assert!(chal.contains("nonce=\"abc123\""), "{chal}");
        // 大小写无关
        assert!(r.header("www-authenticate").is_some());
    }
}
