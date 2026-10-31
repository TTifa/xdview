//! 不开界面，直接验证一路地址能不能出画面。
//!
//! ```bash
//! cargo run -p xd-media --example probe_url -- rtsp://admin:pw@192.168.1.10:554/Streaming/Channels/101
//! cargo run -p xd-media --example probe_url -- https://x/y.m3u8 5
//! ```
//!
//! 第二个参数是要抓的帧数（默认 10）。排查相机地址/密码/编码问题时先用它。

use std::time::{Duration, Instant};
use xd_media::stream::{Event, Stream, StreamOptions};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(url) = args.next() else {
        eprintln!("用法: probe_url <url> [帧数=10]");
        std::process::exit(2);
    };
    let want: usize = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10)
        .max(1);

    // 地址里带账号密码（小度那条还带签名），排查时贴日志也要能直接复制
    println!("目标: {}", xd_core::onvif::mask_url_in_text(&url));
    let net = xd_media::NetOptions {
        no_proxy: true,
        insecure_tls: std::env::var("XD_INSECURE_TLS").is_ok(),
        hwaccel: std::env::var("XD_NO_HWACCEL").is_err(),
    };
    if net.insecure_tls {
        println!("[!] 已开启 -tls_verify 0（跳过证书校验）");
    }
    println!(
        "ffprobe: {:?}",
        xd_media::ffmpeg::probe(
            "ffmpeg",
            &url,
            net,
            &std::sync::atomic::AtomicBool::new(false)
        )
    );

    let opts = StreamOptions {
        url: url.clone(),
        ffmpeg: "ffmpeg".to_string(),
        max_width: 1280,
        low_latency: true,
        reconnect: true,
        net,
    };
    let stream = Stream::start(opts);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut frames = 0usize;
    let mut bytes = 0usize;
    let started = Instant::now();

    while Instant::now() < deadline && frames < want {
        match stream.recv_timeout(Duration::from_millis(500)) {
            Some(Event::Opened {
                width,
                height,
                codec,
            }) => println!("[ok] 打开 {width}x{height} {codec}"),
            Some(Event::Frame(f)) => {
                frames += 1;
                bytes += f.rgba.len();
                println!(
                    "[帧 {frames}] {}x{} seq={} bytes={}",
                    f.width,
                    f.height,
                    f.seq,
                    f.rgba.len()
                );
            }
            Some(Event::Status(s)) => println!("[ffmpeg] {s}"),
            Some(Event::Failed(e)) => {
                eprintln!("[失败] {e}");
                break;
            }
            Some(Event::Closed) => {
                println!("[结束] 拉流线程退出");
                break;
            }
            None => {}
        }
    }

    let secs = started.elapsed().as_secs_f32().max(0.001);
    println!(
        "收到 {frames} 帧, {:.1} MB, {:.1} fps",
        bytes as f32 / 1_048_576.0,
        frames as f32 / secs
    );
    if frames == 0 {
        std::process::exit(1);
    }
}
