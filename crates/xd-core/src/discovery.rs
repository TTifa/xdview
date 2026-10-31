//! WS-Discovery（ONVIF 的局域网发现协议）+ 网段 ONVIF 端点探测。
//!
//! WS-Discovery：组播到 `239.255.255.250:3702` 发一个 `Probe`，设备会单播回
//! `ProbeMatch`，里面带 `XAddrs`（`http://ip/onvif/device_service`）与 `Scopes`。
//!
//! 网段探测（兜底）：小度、TP-Link(Tapo) 等设备**不实现 WS-Discovery**，组播永远
//! 无应答。于是换"主动敲门"：对 /24 网段的常见 ONVIF 端口做 TCP 探活，再向开着的
//! 端口发免鉴权 `GetSystemDateAndTime`（规范里无需账号），回 ONVIF SOAP 即命中。

use crate::http;
use crate::onvif::{elements, soap_envelope};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MULTICAST: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const PORT: u16 = 3702;

/// 网段探测的候选端口：常规 ONVIF(80)、小度(8080)、TP-Link(2020)、
/// 海康(8000)/大华(5000 系)等常见形态。
pub const PROBE_PORTS: &[u16] = &[80, 8080, 2020, 5000, 8899];

#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredDevice {
    /// 设备端点，通常形如 `http://192.168.1.10/onvif/device_service`
    pub endpoint: String,
    /// `dn:NetworkVideoTransmitter` 之类
    pub types: String,
    /// Scope 原文（设备名/型号/位置常在这里）
    pub scopes: String,
}

impl DiscoveredDevice {
    /// 从 endpoint 抽出 `http://host[:port]`，正好是 [`crate::onvif::OnvifClient`] 需要的 base。
    pub fn base_url(&self) -> Option<String> {
        let s = self.endpoint.trim();
        let rest = s.split_once("://")?.1;
        let host_port = rest.split('/').next()?;
        if host_port.is_empty() {
            return None;
        }
        let scheme = s.split_once("://")?.0;
        Some(format!("{scheme}://{host_port}"))
    }

    /// 尽量从 Scopes 里取一个人类可读的名字：`onvif://www.onvif.org/name/门口机`。
    pub fn display_name(&self) -> String {
        for token in self.scopes.split_whitespace() {
            if let Some(name) = token.rsplit('/').next() {
                if token.contains("/name/") && !name.is_empty() {
                    return percent_decode(name);
                }
            }
        }
        self.base_url().unwrap_or_else(|| self.endpoint.clone())
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

pub fn probe_envelope() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope"
            xmlns:w="http://schemas.xmlsoap.org/ws/2004/08/addressing"
            xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery"
            xmlns:dn="http://www.onvif.org/ver10/network/wsdl">
  <e:Header>
    <w:MessageID>uuid:{}</w:MessageID>
    <w:To e:mustUnderstand="true">urn:schemas-xmlsoap-org:ws:2005:04:discovery</w:To>
    <w:Action e:mustUnderstand="true">http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</w:Action>
  </e:Header>
  <e:Body>
    <d:Probe><d:Types>dn:NetworkVideoTransmitter</d:Types></d:Probe>
  </e:Body>
</e:Envelope>"#,
        pseudo_uuid()
    )
}

fn pseudo_uuid() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let a = (nanos & 0xFFFF_FFFF) as u32;
    let b = (nanos >> 32) as u32;
    format!(
        "{:08x}-{:04x}-4{:03x}-a{:03x}-{:012x}",
        a,
        (b & 0xFFFF) as u16,
        ((b >> 16) & 0xFFF) as u16,
        (a & 0xFFF) as u16,
        (nanos >> 16) & 0xFFFF_FFFF_FFFF
    )
}

/// 解析一个 ProbeMatch 报文。抽出来单独可测。
pub fn parse_probe_matches(xml: &str) -> Vec<DiscoveredDevice> {
    let mut out = Vec::new();
    for el in elements(xml, "ProbeMatch") {
        let endpoints = elements(el.inner, "XAddrs");
        let types = elements(el.inner, "Types");
        let scopes = elements(el.inner, "Scopes");
        let xaddrs = endpoints
            .first()
            .map(|e| e.inner.trim().to_string())
            .unwrap_or_default();
        if xaddrs.is_empty() {
            continue;
        }
        // 一台设备可能给多个 XAddr（不同网卡），取第一个
        let endpoint = xaddrs.split_whitespace().next().unwrap_or("").to_string();
        if endpoint.is_empty() {
            continue;
        }
        out.push(DiscoveredDevice {
            endpoint,
            types: types
                .first()
                .map(|e| e.inner.trim().to_string())
                .unwrap_or_default(),
            scopes: scopes
                .first()
                .map(|e| e.inner.trim().to_string())
                .unwrap_or_default(),
        });
    }
    out
}

/// 发一次组播探测并收集应答。
pub fn discover(timeout: Duration) -> Result<Vec<DiscoveredDevice>, String> {
    let sock = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
        .map_err(|e| format!("创建 UDP socket 失败: {e}"))?;
    sock.set_broadcast(true).ok();
    sock.set_read_timeout(Some(Duration::from_millis(200))).ok();

    let target = SocketAddr::from((MULTICAST, PORT));
    let envelope = probe_envelope();
    sock.send_to(envelope.as_bytes(), target)
        .map_err(|e| format!("发送探测包失败: {e}（检查防火墙/是否在同一网段）"))?;

    let deadline = Instant::now() + timeout;
    let mut found: Vec<DiscoveredDevice> = Vec::new();
    let mut buf = vec![0u8; 8192];

    while Instant::now() < deadline {
        match sock.recv_from(&mut buf) {
            Ok((n, _)) => {
                let text = String::from_utf8_lossy(&buf[..n]);
                for d in parse_probe_matches(&text) {
                    if !found.iter().any(|x| x.endpoint == d.endpoint) {
                        found.push(d);
                    }
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => return Err(format!("接收应答失败: {e}")),
        }
    }
    Ok(found)
}

/// 取本机 IPv4（UDP `connect` 到公网只为拿出口地址，不真发包）。
///
/// 注意：VPN/代理的 TUN 网卡（如 Clash 的 198.18.0.1）会劫持默认路由，
/// 拿到的可能不是局域网地址——网段扫描请用 [`local_ipv4_list`]。
pub fn local_ipv4() -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    match sock.local_addr().ok()? {
        SocketAddr::V4(v4) if !v4.ip().is_unspecified() => Some(*v4.ip()),
        _ => None,
    }
}

fn is_private(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 10 || (o[0] == 172 && (16..=31).contains(&o[1])) || (o[0] == 192 && o[1] == 168)
}

/// 本机所有 RFC1918 私网 IPv4（过滤掉 VPN/代理 TUN 的保留段地址）。
///
/// 一台机器可能同时有真实网卡与 TUN 虚拟网卡；扫描只该扫私网段，
/// 否则会像 198.18.0.0/24 这种假网段白白扫一遍。
pub fn local_ipv4_list() -> Vec<Ipv4Addr> {
    let mut out = enum_local_ipv4();
    out.retain(|ip| is_private(*ip));
    out.sort();
    out.dedup();
    out
}

#[cfg(windows)]
fn enum_local_ipv4() -> Vec<Ipv4Addr> {
    /// `MIB_IPADDRROW`（iphlpapi）：一个网卡一条，`addr` 是网络字节序。
    #[repr(C)]
    struct IpAddrRow {
        addr: u32,
        index: u32,
        mask: u32,
        bcast: u32,
        reasm: u32,
        w1: u16,
        typ: u16,
    }
    #[repr(C)]
    struct IpAddrTable {
        num: u32,
        rows: [IpAddrRow; 1],
    }
    #[link(name = "iphlpapi")]
    extern "system" {
        fn GetIpAddrTable(table: *mut IpAddrTable, size: *mut u32, order: i32) -> u32;
    }
    // SAFETY: GetIpAddrTable 按文档两段式调用（先取长度再取表）；
    // 返回 0 时缓冲区内是 dwNumEntries 个连续 IpAddrRow，指针运算只在这段内。
    unsafe {
        let mut size: u32 = 0;
        GetIpAddrTable(std::ptr::null_mut(), &mut size, 1);
        let mut buf = vec![0u8; size.max(64) as usize];
        if GetIpAddrTable(buf.as_mut_ptr() as *mut IpAddrTable, &mut size, 1) != 0 {
            return Vec::new();
        }
        let num =
            (buf[0] as u32) | (buf[1] as u32) << 8 | (buf[2] as u32) << 16 | (buf[3] as u32) << 24;
        let stride = std::mem::size_of::<IpAddrRow>();
        let mut out = Vec::with_capacity(num as usize);
        for i in 0..num as usize {
            let off = 4 + i * stride;
            if off + stride > buf.len() {
                break;
            }
            let mut octets = [0u8; 4];
            octets.copy_from_slice(&buf[off..off + 4]);
            out.push(Ipv4Addr::from(octets));
        }
        out
    }
}

#[cfg(not(windows))]
fn enum_local_ipv4() -> Vec<Ipv4Addr> {
    local_ipv4().into_iter().collect()
}

/// SOAP 回包且带 ONVIF 痕迹才算设备（挡掉端口上的普通网页/路由器界面）。
fn looks_like_onvif(body: &str) -> bool {
    body.contains("Envelope") && (body.contains("SystemDateAndTime") || body.contains("onvif.org"))
}

/// 免鉴权 ONVIF 探活：POST `GetSystemDateAndTime`（规范里无需账号），
/// 回了 ONVIF SOAP 即命中。路径先试约定的 `/onvif/device_service`，
/// 再试 gSOAP 单端点 `/onvif/service`（TP-Link 形态，见踩坑 #12）。
pub fn probe_onvif_endpoint(base: &str, timeout: Duration) -> Option<DiscoveredDevice> {
    let body = soap_envelope(
        "",
        r#"<tds:GetSystemDateAndTime xmlns:tds="http://www.onvif.org/ver10/device/wsdl"/>"#,
    );
    let headers = [("Content-Type", "application/soap+xml; charset=utf-8")];
    let base = base.trim_end_matches('/');
    for path in ["/onvif/device_service", "/onvif/service"] {
        let endpoint = format!("{base}{path}");
        if let Ok(resp) = http::post(&endpoint, &headers, body.as_bytes(), timeout) {
            if looks_like_onvif(&resp.body_text()) {
                return Some(DiscoveredDevice {
                    endpoint,
                    types: "probe:GetSystemDateAndTime".into(),
                    scopes: String::new(),
                });
            }
        }
    }
    None
}

/// 扫本机 /24 网段的候选端口，返回确认为 ONVIF 的设备。
///
/// 两阶段：先并发 TCP 探活列出开端口（死 IP 的 ARP 超时压在 150ms 连接超时里），
/// 再只对开着的端口做 SOAP 确认——普通网页不会误报成摄像机。
pub fn scan_subnet(ip: Ipv4Addr, ports: &[u16]) -> Vec<DiscoveredDevice> {
    use std::sync::mpsc;
    use std::thread;

    let base = u32::from(ip) & 0xFFFF_FF00;
    let hosts: Vec<Ipv4Addr> = (1..=254u32).map(|h| Ipv4Addr::from(base | h)).collect();

    // 阶段一：TCP 探活，64 个工人按 host 分块
    let (tx, rx) = mpsc::channel();
    let mut workers = Vec::new();
    for chunk in hosts.chunks(4) {
        let chunk = chunk.to_vec();
        let tx = tx.clone();
        let ports = ports.to_vec();
        workers.push(thread::spawn(move || {
            for host in chunk {
                for port in &ports {
                    let addr = SocketAddr::from((host, *port));
                    if TcpStream::connect_timeout(&addr, Duration::from_millis(150)).is_ok() {
                        let _ = tx.send((host, *port));
                    }
                }
            }
        }));
    }
    drop(tx);
    let open: Vec<(Ipv4Addr, u16)> = rx.iter().collect();
    for w in workers {
        let _ = w.join();
    }

    // 阶段二：对开着的端口并行 SOAP 确认
    let (tx2, rx2) = mpsc::channel();
    let mut workers = Vec::new();
    for (host, port) in open {
        let tx2 = tx2.clone();
        workers.push(thread::spawn(move || {
            let base = format!("http://{host}:{port}");
            if let Some(d) = probe_onvif_endpoint(&base, Duration::from_millis(800)) {
                let _ = tx2.send(d);
            }
        }));
    }
    drop(tx2);
    let mut out: Vec<DiscoveredDevice> = rx2.iter().collect();
    for w in workers {
        let _ = w.join();
    }
    out.sort_by(|a, b| a.endpoint.cmp(&b.endpoint));
    out.dedup_by(|a, b| a.endpoint == b.endpoint);
    dedup_by_host(out)
}

/// 同一 IP 多端口命中（如 TP-Link 的 80 网页端点 + 2020 ONVIF）按一台算，
/// 优先留非 80 的专用端口。
fn dedup_by_host(out: Vec<DiscoveredDevice>) -> Vec<DiscoveredDevice> {
    let host_of = |d: &DiscoveredDevice| -> Option<String> {
        d.base_url().and_then(|b| {
            b.split("://")
                .nth(1)
                .map(|h| h.split(':').next().unwrap_or("").to_string())
        })
    };
    let port_of = |d: &DiscoveredDevice| -> u16 {
        d.base_url()
            .and_then(|b| {
                let h = b.split("://").nth(1)?;
                h.split(':').nth(1)?.parse::<u16>().ok()
            })
            .unwrap_or(80)
    };
    let mut by_host: Vec<DiscoveredDevice> = Vec::new();
    for d in out {
        match host_of(&d).and_then(|h| {
            by_host
                .iter()
                .position(|x| host_of(x).as_deref() == Some(h.as_str()))
        }) {
            Some(pos) => {
                if port_of(&by_host[pos]) == 80 && port_of(&d) != 80 {
                    by_host[pos] = d;
                }
            }
            None => by_host.push(d),
        }
    }
    by_host
}

/// 扫本机所有私网网段的候选端口（[`scan_subnet`] 的多网段入口）。
pub fn scan_lan(ports: &[u16]) -> Vec<DiscoveredDevice> {
    let mut out = Vec::new();
    for ip in local_ipv4_list() {
        for d in scan_subnet(ip, ports) {
            let dup = d
                .base_url()
                .map(|b| {
                    out.iter()
                        .any(|x: &DiscoveredDevice| x.base_url().as_deref() == Some(b.as_str()))
                })
                .unwrap_or(false);
            if !dup {
                out.push(d);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope"
 xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery">
 <SOAP-ENV:Body>
  <d:ProbeMatches>
   <d:ProbeMatch>
    <d:EndpointReference><a:Address>urn:uuid:1234</a:Address></d:EndpointReference>
    <d:Types>dn:NetworkVideoTransmitter tds:Device</d:Types>
    <d:Scopes>onvif://www.onvif.org/type/video_encoder onvif://www.onvif.org/name/DoorCam onvif://www.onvif.org/location/hall</d:Scopes>
    <d:XAddrs>http://192.168.1.64/onvif/device_service</d:XAddrs>
    <d:MetadataVersion>1</d:MetadataVersion>
   </d:ProbeMatch>
  </d:ProbeMatches>
 </SOAP-ENV:Body>
</SOAP-ENV:Envelope>"#;

    #[test]
    fn parses_probe_match() {
        let list = parse_probe_matches(SAMPLE);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].endpoint, "http://192.168.1.64/onvif/device_service");
        assert_eq!(list[0].base_url().as_deref(), Some("http://192.168.1.64"));
        assert_eq!(list[0].display_name(), "DoorCam");
        assert!(list[0].types.contains("NetworkVideoTransmitter"));
    }

    #[test]
    fn ignores_match_without_xaddrs() {
        let xml = r#"<d:ProbeMatch><d:Types>x</d:Types></d:ProbeMatch>"#;
        assert!(parse_probe_matches(xml).is_empty());
    }

    #[test]
    fn decodes_percent_encoded_name() {
        let d = DiscoveredDevice {
            endpoint: "http://h/onvif/device_service".into(),
            types: String::new(),
            scopes: "onvif://www.onvif.org/name/%E9%97%A8%E5%8F%A3".into(),
        };
        assert_eq!(d.display_name(), "门口");
    }

    #[test]
    fn falls_back_to_base_url_when_no_scope_name() {
        let d = DiscoveredDevice {
            endpoint: "http://10.0.0.2:8080/onvif/device_service".into(),
            types: String::new(),
            scopes: "onvif://www.onvif.org/type/x".into(),
        };
        assert_eq!(d.display_name(), "http://10.0.0.2:8080");
    }

    #[test]
    fn probe_envelope_has_required_fields() {
        let e = probe_envelope();
        assert!(e.contains("Probe"));
        assert!(e.contains("NetworkVideoTransmitter"));
        assert!(e.contains("urn:schemas-xmlsoap-org:ws:2005:04:discovery"));
        assert!(e.contains("<w:MessageID>uuid:"));
    }

    #[test]
    fn private_range_filter_drops_tun_addresses() {
        assert!(is_private(Ipv4Addr::new(192, 168, 68, 10)));
        assert!(is_private(Ipv4Addr::new(10, 0, 0, 2)));
        assert!(is_private(Ipv4Addr::new(172, 16, 5, 5)));
        // Clash/TUN 保留段、链路本地、公网都不该扫
        assert!(!is_private(Ipv4Addr::new(198, 18, 0, 1)));
        assert!(!is_private(Ipv4Addr::new(169, 254, 1, 1)));
        assert!(!is_private(Ipv4Addr::new(8, 8, 8, 8)));
    }

    #[test]
    fn dedup_by_host_prefers_non_80_port() {
        let mk = |ep: &str| DiscoveredDevice {
            endpoint: ep.into(),
            types: String::new(),
            scopes: String::new(),
        };
        let list = dedup_by_host(vec![
            mk("http://192.168.1.10:80/onvif/device_service"),
            mk("http://192.168.1.10:2020/onvif/device_service"),
            mk("http://192.168.1.20:8080/onvif/device_service"),
        ]);
        assert_eq!(list.len(), 2);
        assert!(list[0].endpoint.contains(":2020"), "同 IP 应保留非 80 端口");
        assert!(list[1].endpoint.contains("192.168.1.20"));
    }

    #[test]
    fn looks_like_onvif_needs_soap_and_marker() {
        assert!(looks_like_onvif(
            "<SOAP-ENV:Envelope><GetSystemDateAndTimeResponse/></SOAP-ENV:Envelope>"
        ));
        assert!(looks_like_onvif(
            "<env:Envelope xmlns:t=\"www.onvif.org/ver10/error\"><env:Fault><env:Code>ter:NotAuthorized</env:Code></env:Fault></env:Envelope>"
        ));
        assert!(!looks_like_onvif("<html><body>路由器管理页</body></html>"));
        assert!(!looks_like_onvif("SystemDateAndTime 但没有 SOAP 信封"));
    }

    /// 起一个一次性 HTTP stub：`response` 为完整 HTTP 应答，可应答多次。
    fn spawn_stub(response: &'static str, times: usize) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind stub");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(times) {
                let Ok(mut s) = stream else { continue };
                let mut buf = [0u8; 4096];
                let _ = std::io::Read::read(&mut s, &mut buf);
                let _ = std::io::Write::write_all(&mut s, response.as_bytes());
            }
        });
        port
    }

    const ONVIF_OK: &str = "HTTP/1.1 200 OK\r\nContent-Type: application/soap+xml\r\nContent-Length: 123\r\nConnection: close\r\n\r\n<S:Envelope xmlns:S=\"http://www.w3.org/2003/05/soap-envelope\"><S:Body><GetSystemDateAndTimeResponse/></S:Body></S:Envelope>";
    const PLAIN_HTML: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello";

    #[test]
    fn probe_accepts_onvif_soap() {
        let port = spawn_stub(ONVIF_OK, 1);
        let d = probe_onvif_endpoint(&format!("http://127.0.0.1:{port}"), Duration::from_secs(2))
            .expect("应识别为 ONVIF 设备");
        assert!(d.endpoint.ends_with("/onvif/device_service"));
        assert_eq!(
            d.base_url().as_deref(),
            Some(format!("http://127.0.0.1:{port}").as_str())
        );
    }

    #[test]
    fn probe_rejects_plain_http() {
        // 探测会依次打 /onvif/device_service 与 /onvif/service 两发，stub 应答两次
        let port = spawn_stub(PLAIN_HTML, 2);
        assert!(
            probe_onvif_endpoint(&format!("http://127.0.0.1:{port}"), Duration::from_secs(2))
                .is_none()
        );
    }
}
