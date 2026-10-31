//! ONVIF 客户端联调：起一个假 ONVIF 服务（TcpListener），验证
//! SOAP 请求构造 → 最小 HTTP 客户端 → XML 解析这条链路。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use xd_core::onvif::{Credentials, OnvifClient};

/// 只回一次请求的假服务，返回固定 XML。
fn serve_once(response_body: &'static str) -> (String, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定端口失败");
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept 失败");
        // 头部与 body 可能分两个 TCP 段到达，必须按 Content-Length 读满
        let mut data: Vec<u8> = Vec::new();
        let mut buf = [0u8; 4096];
        while !request_complete(&data) {
            match sock.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => data.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        let request = String::from_utf8_lossy(&data).to_string();
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/soap+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        let _ = sock.write_all(resp.as_bytes());
        let _ = sock.flush();
        request
    });
    (format!("http://{addr}"), handle)
}

/// 是否已收到完整请求（头部 + Content-Length 指定的 body）。
fn request_complete(data: &[u8]) -> bool {
    let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&data[..pos]).to_ascii_lowercase();
    let len = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    data.len() >= pos + 4 + len
}

#[test]
fn parses_profiles_from_fake_service() {
    let xml = r#"<?xml version="1.0"?>
<trt:GetProfilesResponse xmlns:trt="http://www.onvif.org/ver10/media/wsdl">
  <trt:Profiles token="Profile_1" fixed="true">
    <tt:Name xmlns:tt="http://www.onvif.org/ver10/schema">mainStream</tt:Name>
  </trt:Profiles>
  <trt:Profiles token="Profile_2">
    <tt:Name xmlns:tt="http://www.onvif.org/ver10/schema">subStream</tt:Name>
  </trt:Profiles>
</trt:GetProfilesResponse>"#;

    let (base, handle) = serve_once(xml);
    let client = OnvifClient::new(
        base,
        Some(Credentials {
            username: "admin".into(),
            password: "pw".into(),
        }),
    );
    let profiles = client.get_profiles().expect("应解析出 profile");
    assert_eq!(profiles.len(), 2);
    assert_eq!(profiles[0].token, "Profile_1");
    assert_eq!(profiles[0].name, "mainStream");
    assert_eq!(profiles[1].token, "Profile_2");

    let request = handle.join().unwrap();
    // 校验请求本身：路径、POST、SOAP 头、WS-Security 都在
    assert!(
        request.starts_with("POST /onvif/media_service HTTP/1.1"),
        "{request}"
    );
    assert!(request.contains("Content-Type: application/soap+xml"));
    assert!(request.contains("UsernameToken"));
    assert!(request.contains("<Username>admin</Username>"));
    assert!(request.contains("PasswordDigest"));
    assert!(request.contains("GetProfiles"));
}

#[test]
fn continuous_move_posts_velocity() {
    let xml = r#"<?xml version="1.0"?><trt:ContinuousMoveResponse xmlns:trt="http://www.onvif.org/ver20/ptz/wsdl"/>"#;
    let (base, handle) = serve_once(xml);
    let client = OnvifClient::new(base, None);
    client
        .continuous_move(
            "Profile_1",
            xd_core::onvif::Velocity {
                pan: 0.5,
                tilt: -0.25,
                zoom: 0.0,
            },
            None,
        )
        .expect("ContinuousMove 应成功");

    let request = handle.join().unwrap();
    assert!(
        request.starts_with("POST /onvif/ptz_service HTTP/1.1"),
        "{request}"
    );
    assert!(request.contains("<trt:ProfileToken>Profile_1</trt:ProfileToken>"));
    assert!(
        request.contains(r#"<tt:PanTilt x="0.500" y="-0.250""#),
        "{request}"
    );
    assert!(
        !request.contains("UsernameToken"),
        "无凭据时不应带 WS-Security"
    );
}

#[test]
fn continuous_move_posts_zoom_axis() {
    let xml = r#"<?xml version="1.0"?><trt:ContinuousMoveResponse xmlns:trt="http://www.onvif.org/ver20/ptz/wsdl"/>"#;
    let (base, handle) = serve_once(xml);
    let client = OnvifClient::new(base, None);
    client
        .continuous_move(
            "Profile_1",
            xd_core::onvif::Velocity {
                pan: 0.0,
                tilt: 0.0,
                zoom: 0.7,
            },
            None,
        )
        .expect("纯变焦 ContinuousMove 应成功");
    let request = handle.join().unwrap();
    assert!(
        request.contains(r#"<tt:Zoom x="0.700""#),
        "变焦轴必须进 SOAP：{request}"
    );
}

#[test]
fn set_preset_returns_token_and_posts_name() {
    let xml = r#"<?xml version="1.0"?><trt:SetPresetResponse xmlns:trt="http://www.onvif.org/ver20/ptz/wsdl">
<trt:PresetToken>Preset_7</trt:PresetToken></trt:SetPresetResponse>"#;
    let (base, handle) = serve_once(xml);
    let client = OnvifClient::new(base, None);
    let token = client
        .set_preset("Profile_1", "门口全景", None)
        .expect("SetPreset 应成功");
    assert_eq!(token, "Preset_7");
    let request = handle.join().unwrap();
    assert!(request.contains("<trt:SetPreset"), "{request}");
    assert!(
        request.contains("<trt:PresetName>门口全景</trt:PresetName>"),
        "{request}"
    );
}

#[test]
fn remove_preset_posts_token() {
    let xml = r#"<?xml version="1.0"?><trt:RemovePresetResponse xmlns:trt="http://www.onvif.org/ver20/ptz/wsdl"/>"#;
    let (base, handle) = serve_once(xml);
    let client = OnvifClient::new(base, None);
    client
        .remove_preset("Profile_1", "Preset_3")
        .expect("RemovePreset 应成功");
    let request = handle.join().unwrap();
    assert!(request.contains("<trt:RemovePreset"), "{request}");
    assert!(
        request.contains("<trt:PresetToken>Preset_3</trt:PresetToken>"),
        "{request}"
    );
}

#[test]
fn resolves_stream_uri_and_injects_credentials() {
    let xml = r#"<?xml version="1.0"?><trt:GetStreamUriResponse xmlns:trt="http://www.onvif.org/ver10/media/wsdl">
<trt:MediaUri><tt:Uri xmlns:tt="http://www.onvif.org/ver10/schema">rtsp://192.168.1.64:554/Streaming/Channels/101</tt:Uri>
<tt:InvalidAfterConnect>false</tt:InvalidAfterConnect><tt:Timeout>PT0S</tt:Timeout></trt:MediaUri>
</trt:GetStreamUriResponse>"#;
    let (base, handle) = serve_once(xml);
    let client = OnvifClient::new(base, None);
    let uri = client.get_stream_uri("Profile_1").expect("应拿到取流地址");
    assert_eq!(uri, "rtsp://192.168.1.64:554/Streaming/Channels/101");

    let full = xd_core::onvif::inject_credentials(&uri, "admin", "p@ss");
    assert_eq!(
        full,
        "rtsp://admin:p%40ss@192.168.1.64:554/Streaming/Channels/101"
    );

    let request = handle.join().unwrap();
    assert!(
        request.starts_with("POST /onvif/media_service HTTP/1.1"),
        "{request}"
    );
    assert!(request.contains("GetStreamUri"));
    assert!(
        request.contains("<tt:Protocol>RTSP</tt:Protocol>"),
        "{request}"
    );
}

#[test]
fn falls_back_to_reported_service_endpoint_on_404() {
    // TP-Link 这类 gSOAP 实现把全部服务挂在单一 /onvif/service 上。
    // 约定路径先撞 404 → 问 GetCapabilities 要 XAddr → 换地址重试。
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定端口失败");
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");

    let caps_body = format!(
        r#"<?xml version="1.0"?>
<tds:GetCapabilitiesResponse xmlns:tds="http://www.onvif.org/ver10/device/wsdl">
  <tds:Capabilities>
    <tt:Media xmlns:tt="http://www.onvif.org/ver10/schema"><tt:XAddr>{base}/onvif/service</tt:XAddr></tt:Media>
    <tt:PTZ xmlns:tt="http://www.onvif.org/ver10/schema"><tt:XAddr>{base}/onvif/service</tt:XAddr></tt:PTZ>
  </tds:Capabilities>
</tds:GetCapabilitiesResponse>"#
    );
    let profiles_body = r#"<?xml version="1.0"?>
<trt:GetProfilesResponse xmlns:trt="http://www.onvif.org/ver10/media/wsdl">
  <trt:Profiles token="MainStream"><tt:Name xmlns:tt="http://www.onvif.org/ver10/schema">MainStream</tt:Name></trt:Profiles>
</trt:GetProfilesResponse>"#
        .to_string();

    let responses = vec![
        (404u16, String::new()),
        (200u16, caps_body),
        (200u16, profiles_body),
    ];

    let handle = thread::spawn(move || {
        let mut seen: Vec<String> = Vec::new();
        for (status, body) in responses {
            let Ok((mut sock, _)) = listener.accept() else {
                break;
            };
            let mut data: Vec<u8> = Vec::new();
            let mut buf = [0u8; 4096];
            while !request_complete(&data) {
                match sock.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => data.extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
            }
            seen.push(String::from_utf8_lossy(&data).to_string());
            let reason = if status == 200 { "OK" } else { "Not Found" };
            let resp = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/soap+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes());
            let _ = sock.flush();
        }
        seen
    });

    let client = OnvifClient::new(base, None);
    let got = client.get_profiles().expect("应通过回退端点拿到 profile");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].token, "MainStream");

    let seen = handle.join().unwrap();
    let paths: Vec<&str> = seen
        .iter()
        .map(|r| r.split_whitespace().nth(1).unwrap_or(""))
        .collect();
    assert_eq!(
        paths,
        vec![
            "/onvif/media_service",
            "/onvif/device_service",
            "/onvif/service"
        ],
        "应为：约定路径 404 → 取能力 → 回退端点重试"
    );
}
