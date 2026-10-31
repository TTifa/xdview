//! 滚动缓存端到端：真起 ffmpeg 切片、校验片段可播、再把多片拼回一个文件。
//!
//! 需要本机有 ffmpeg，默认 `#[ignore]`，跑法：
//! `cargo test -p xd-media --test cache -- --ignored --nocapture`

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use xd_media::cache::{self, CacheOptions, SegmentCache};
use xd_media::NetOptions;

fn tmp_dir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("xdview_cache_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// 造一段带关键帧间隔的测试视频。
fn make_clip(dir: &Path, name: &str, secs: u32) -> PathBuf {
    let out = dir.join(name);
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=10",
            "-t",
            &secs.to_string(),
            "-g",
            "10",
            "-keyint_min",
            "10",
            "-sc_threshold",
            "0",
            "-pix_fmt",
            "yuv420p",
            &out.to_string_lossy(),
        ])
        .status()
        .expect("需要 ffmpeg");
    assert!(status.success());
    out
}

fn probe_seconds(path: &Path) -> f64 {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
            &path.to_string_lossy(),
        ])
        .output()
        .expect("需要 ffprobe");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0.0)
}

#[test]
#[ignore]
fn cache_writes_playable_segment() {
    let dir = tmp_dir("write");
    let clip = make_clip(&dir, "clip.mp4", 3);
    let cache_dir = dir.join("cache").join("cam1");

    let mut c = SegmentCache::start(CacheOptions {
        ffmpeg: "ffmpeg".into(),
        url: clip.to_string_lossy().to_string(),
        dir: cache_dir.clone(),
        segment_secs: 60,
        keep_secs: 3600,
        net: NetOptions::default(),
    })
    .expect("启动缓存");

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if c.poll().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }

    let files = cache::list_segments(&cache_dir);
    println!("片段: {files:?}");
    assert!(!files.is_empty(), "应至少产出一个片段");
    assert!(files.iter().all(|(_, size, _)| *size > 0), "不应有空片段");

    let usage = c.usage();
    assert_eq!(usage.files, files.len());
    assert!(usage.bytes > 0);

    // 片段必须能读、且有正时长（说明 -c copy 出来的容器是完整的）
    let dur = probe_seconds(&files[0].0);
    println!("首个片段时长: {dur:.2}s");
    assert!(dur > 1.0, "片段时长异常: {dur}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore]
fn export_concats_segments_in_order() {
    let dir = tmp_dir("export");
    let cache_dir = dir.join("cache").join("cam2");
    std::fs::create_dir_all(&cache_dir).unwrap();

    // 手工造三个「片段」，名字按时间递增，验证拼接顺序与总时长
    for (i, secs) in [1u32, 2, 1].iter().enumerate() {
        let clip = make_clip(&dir, &format!("src{i}.mp4"), *secs);
        std::fs::copy(&clip, cache_dir.join(format!("cam2_20261031-15300{i}.mkv"))).unwrap();
    }

    let files = cache::list_segments(&cache_dir);
    assert_eq!(files.len(), 3);

    let out = dir.join("out").join("merged.mkv");
    let n =
        cache::export("ffmpeg", &cache_dir, 3600, &out, NetOptions::default()).expect("导出应成功");
    println!("导出 {n} 片 -> {}", out.display());
    assert_eq!(n, 3);

    let total = probe_seconds(&out);
    println!("拼接后时长: {total:.2}s");
    // 1 + 2 + 1 = 4 秒，允许容器层的小误差
    assert!(total > 3.0 && total < 5.5, "拼接时长异常: {total}");

    // 时间窗过滤：只要最近 1 秒内的片段
    let picked = cache::segments_since(&files, u64::MAX, 1);
    assert_eq!(picked.len(), 1, "窗口过滤应只留最新一个");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 对**真实相机**验证「停止时是否优雅收尾」：
/// 最后一片的时长应接近实际录制时长，而不是被 ffprobe 按码率瞎估。
///
/// 跑法：`XD_TEST_RTSP=rtsp://user:pass@ip:554/stream1 cargo test -p xd-media --test cache -- --ignored --nocapture stop_finalizes`
#[test]
#[ignore]
fn stop_finalizes_last_segment() {
    let Ok(url) = std::env::var("XD_TEST_RTSP") else {
        println!("跳过：未设置 XD_TEST_RTSP");
        return;
    };
    let dir = tmp_dir("stop");
    let cache_dir = dir.join("cache").join("live");

    let c = SegmentCache::start(CacheOptions {
        ffmpeg: "ffmpeg".into(),
        url,
        dir: cache_dir.clone(),
        segment_secs: 600, // 故意设很长：保证停止时当前片段还没切完
        keep_secs: 3600,
        net: NetOptions::default(),
    })
    .expect("启动缓存");

    std::thread::sleep(Duration::from_secs(8));
    c.stop(); // 走 'q' 优雅收尾

    let files = cache::list_segments(&cache_dir);
    assert_eq!(files.len(), 1, "8 秒内不应切片");
    let dur = probe_seconds(&files[0].0);
    println!("最后一片时长: {dur:.2}s（录制约 8 秒）");
    assert!(
        dur > 4.0 && dur < 15.0,
        "优雅收尾后时长应接近实际录制长度，实际 {dur}s —— 说明 'q' 没生效，文件被硬截断"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
