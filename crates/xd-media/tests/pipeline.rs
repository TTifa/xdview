//! 端到端拉流测试：真的起一个 ffmpeg 子进程、真的按 rawvideo 读帧。
//!
//! 需要本机有 `ffmpeg`，所以默认 `#[ignore]`，用
//! `cargo test -p xd-media --test pipeline -- --ignored --nocapture` 跑。

use std::process::Command;
use std::time::{Duration, Instant};
use xd_media::stream::{Event, Stream, StreamOptions};

#[test]
#[ignore = "需要本机安装 ffmpeg"]
fn decodes_generated_video() {
    let dir = std::env::temp_dir().join("xdview_pipeline_test");
    let _ = std::fs::create_dir_all(&dir);
    let clip = dir.join("testsrc.mp4");
    let _ = std::fs::remove_file(&clip);

    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=10",
            "-t",
            "2",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&clip)
        .status()
        .expect("无法启动 ffmpeg，请确认已安装");
    assert!(status.success(), "生成测试视频失败");

    let opts = StreamOptions {
        url: clip.to_string_lossy().to_string(),
        ffmpeg: "ffmpeg".to_string(),
        max_width: 320,
        low_latency: true,
        reconnect: false,
        net: xd_media::NetOptions::default(),
    };
    let stream = Stream::start(opts);

    let mut opened = false;
    let mut frames = 0usize;
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        match stream.recv_timeout(Duration::from_millis(500)) {
            Some(Event::Opened {
                width,
                height,
                codec,
            }) => {
                assert_eq!((width, height), (320, 240), "尺寸应与探测一致");
                println!("opened {width}x{height} codec={codec}");
                opened = true;
            }
            Some(Event::Frame(f)) => {
                assert_eq!(f.rgba.len(), 320 * 240 * 4, "帧长度必须是 w*h*4");
                assert_eq!(f.width, 320);
                frames += 1;
                if frames >= 5 {
                    break;
                }
            }
            Some(Event::Failed(e)) => panic!("拉流失败: {e}"),
            Some(Event::Closed) => break,
            Some(Event::Status(s)) => println!("status: {s}"),
            None => {}
        }
    }
    assert!(opened, "没有收到 Opened 事件");
    assert!(frames >= 5, "帧数不足，只收到 {frames} 帧");
    let _ = std::fs::remove_file(&clip);
}
