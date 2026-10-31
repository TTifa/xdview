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
        let (scheme, rest) = raw
            .split_once("://")
            .ok_or_else(|| format!("URL 缺少 scheme: {raw}"))?;
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
            return Err(format!("URL 缺少主机: {raw}"));
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

pub fn post(
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    timeout: Duration,
) -> Result<Response, String> {
    let u = Url::parse(url)?;
    let addr = format!("{}:{}", u.host, u.port);
    let sock = addr
        .to_socket_addrs()
        .map_err(|e| format!("解析 {addr} 失败: {e}"))?
        .next()
        .ok_or_else(|| format!("{addr} 无可用地址"))?;
    let mut stream =
        TcpStream::connect_timeout(&sock, timeout).map_err(|e| format!("连接 {addr} 失败: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| format!("设置读超时失败: {e}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| format!("设置写超时失败: {e}"))?;

    let mut req = String::new();
    req.push_str(&format!("POST {} HTTP/1.1\r\n", u.path));
    req.push_str(&format!("Host: {}\r\n", u.authority()));
    req.push_str("Connection: close\r\n");
    req.push_str("User-Agent: xdview/0.1\r\n");
    req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");

    stream
        .write_all(req.as_bytes())
        .and_then(|_| stream.write_all(body))
        .and_then(|_| stream.flush())
        .map_err(|e| format!("发送请求失败: {e}"))?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > MAX_BODY {
                    return Err("响应体过大".into());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            // 部分设备不回 Connection: close，读超时即视为结束
            Err(_) => break,
        }
    }

    parse_response(&raw)
}

/// 找到真正的 HTTP 状态行起点（`HTTP/1.x NNN`）。
///
/// 部分设备（TP-Link 拒绝变焦指令实测）会先回显请求/塞一段垃圾再回响应，
/// 直接拿首行当状态行会解析失败，这里按「HTTP/1.x + 空格 + 三位状态码」定位。
fn find_status_line_start(raw: &[u8]) -> Option<usize> {
    let pat = b"HTTP/1.";
    let mut idx = 0;
    while let Some(pos) = find_subslice(&raw[idx..], pat) {
        let abs = idx + pos;
        let tail = raw.get(abs + 7..abs + 12);
        if let Some(t) = tail {
            let looks_like_status = matches!(t[0], b'0' | b'1')
                && t[1] == b' '
                && t[2..].iter().all(|b| b.is_ascii_digit());
            if looks_like_status {
                return Some(abs);
            }
        }
        idx = abs + 1;
    }
    None
}

fn parse_response(raw: &[u8]) -> Result<Response, String> {
    if raw.is_empty() {
        return Err("空响应".into());
    }
    let raw = match find_status_line_start(raw) {
        Some(start) => &raw[start..],
        None => raw,
    };
    let split = find_subslice(raw, b"\r\n\r\n").ok_or("响应缺少头部结束标记")?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let body_raw = &raw[split + 4..];

    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let mut parts = status_line.splitn(3, ' ');
    let _ver = parts.next().unwrap_or("");
    let status: u16 = parts
        .next()
        .unwrap_or("0")
        .parse()
        .map_err(|_| format!("状态行非法: {status_line}"))?;
    let reason = parts.next().unwrap_or("").to_string();

    let mut chunked = false;
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let (k, v) = (k.trim().to_string(), v.trim().to_string());
            if k.eq_ignore_ascii_case("transfer-encoding")
                && v.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            }
            headers.push((k, v));
        }
    }

    let body = if chunked {
        decode_chunked(body_raw)?
    } else {
        body_raw.to_vec()
    };

    Ok(Response {
        status,
        reason,
        body,
        headers,
    })
}

fn decode_chunked(mut data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let line_end = find_subslice(data, b"\r\n").ok_or("chunked: 缺少长度行")?;
        let line = String::from_utf8_lossy(&data[..line_end]).to_string();
        let size_str = line.split(';').next().unwrap_or("").trim().to_string();
        let size = usize::from_str_radix(&size_str, 16)
            .map_err(|_| format!("chunked: 长度非法 {size_str}"))?;
        data = &data[line_end + 2..];
        if size == 0 {
            break;
        }
        if data.len() < size {
            out.extend_from_slice(data);
            break;
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size..];
        if data.starts_with(b"\r\n") {
            data = &data[2..];
        }
    }
    Ok(out)
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
