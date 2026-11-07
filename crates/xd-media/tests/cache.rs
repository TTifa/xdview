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
#[ignore = "需要 Windows 和本机 ffmpeg；导出屏障与真实清理联调"]
#[cfg(windows)]
fn export_keeps_expired_segments_until_ffmpeg_finishes() {
    use std::sync::atomic::AtomicBool;
    let dir = tmp_dir("export_prune");
    let clip = make_clip(&dir, "clip.mkv", 1);
    let source = dir.join("segments");
    std::fs::create_dir(&source).unwrap();
    for name in ["a.mkv", "b.mkv", "y.mkv", "z.mkv"] {
        let path = source.join(name);
        std::fs::copy(&clip, &path).unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() - Duration::from_secs(60)),
        )
        .unwrap();
    }
    let mut cache = SegmentCache::start(CacheOptions {
        ffmpeg: "ffmpeg".into(),
        url: clip.to_string_lossy().into(),
        dir: source.clone(),
        segment_secs: 5,
        keep_secs: 1,
        net: NetOptions::default(),
    })
    .unwrap();
    // 本用例只验证清理竞争，先让有限合成输入写完，排除未收尾尾片的影响。
    let deadline = Instant::now() + Duration::from_secs(5);
    while cache.poll().is_none() {
        assert!(Instant::now() < deadline, "合成缓存未完成");
        std::thread::sleep(Duration::from_millis(10));
    }
    let ready = dir.join("ready");
    let release = dir.join("release");
    let wrapper = dir.join("barrier.cmd");
    std::fs::write(&wrapper, format!("@echo off\r\necho ready>\"{}\"\r\n:wait\r\nif not exist \"{}\" goto wait\r\nffmpeg %*\r\n", ready.display(), release.display())).unwrap();
    let out = dir.join("export.mkv");
    let export_source = source.clone();
    let export_out = out.clone();
    let task = std::thread::spawn(move || {
        cache::export_cancellable(
            &wrapper.to_string_lossy(),
            &export_source,
            120,
            &export_out,
            NetOptions::default(),
            &AtomicBool::new(false),
            Duration::from_secs(10),
        )
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "导出未到达读取屏障");
        std::thread::sleep(Duration::from_millis(10));
    }
    cache.prune();
    let retained = source.join("a.mkv").exists() && source.join("b.mkv").exists();
    // 先释放屏障并回收任务，再断言，避免回归失败时遗留忙等进程。
    std::fs::write(release, b"go").unwrap();
    let result = task.join().unwrap();
    assert!(retained, "清理不能删除导出尚未读取的过期片段");
    assert!(result.unwrap() >= 4);
    assert!(probe_seconds(&out) >= 3.9, "导出必须包含全部合成片段");
    cache.prune();
    assert!(!source.join("a.mkv").exists(), "导出结束后应释放保留");
    cache.stop();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "需要本机安装 ffmpeg；Windows 验证 q 收尾"]
#[cfg(windows)]
fn recorder_finishes_playable_file_and_reports_timeout() {
    let dir = tmp_dir("recorder");
    let clip = make_clip(&dir, "clip.mp4", 8);
    let paced = dir.join("paced.cmd");
    std::fs::write(&paced, "@echo off\r\nffmpeg -re %*\r\n").unwrap();
    let out = dir.join("record.mkv");
    let recorder = xd_media::Recorder::start(
        &paced.to_string_lossy(),
        &clip.to_string_lossy(),
        &out,
        false,
        NetOptions::default(),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(1400));
    let started = Instant::now();
    assert_eq!(recorder.stop().unwrap(), out);
    assert!(started.elapsed() < Duration::from_secs(3));
    let seconds = probe_seconds(&out);
    assert!(
        seconds > 0.0 && seconds < 8.0,
        "应保存部分录像并写好时长：{seconds}"
    );
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&out)
        .args(["-f", "null", "-"])
        .status()
        .unwrap();
    assert!(status.success(), "收尾后的录像应可解码");

    let stuck = dir.join("stuck.cmd");
    std::fs::write(
        &stuck,
        "@echo off\r\n:loop\r\nset /a x=1 > nul\r\ngoto loop\r\n",
    )
    .unwrap();
    let recorder = xd_media::Recorder::start(
        &stuck.to_string_lossy(),
        "unused",
        &dir.join("bad.mkv"),
        false,
        NetOptions::default(),
    )
    .unwrap();
    let started = Instant::now();
    assert!(recorder.stop().unwrap_err().contains("超时"));
    assert!(started.elapsed() < Duration::from_secs(4));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "Windows 专用超时/取消进程 fixture"]
#[cfg(windows)]
fn export_cancels_and_times_out_without_overwriting_existing_file() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let dir = tmp_dir("export_cancel");
    let source = dir.join("segments");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("fake.mkv"), b"test").unwrap();
    let out = dir.join("keep.mkv");
    std::fs::write(&out, b"original").unwrap();
    let stuck = dir.join("stuck.cmd");
    std::fs::write(
        &stuck,
        "@echo off\r\n:loop\r\nset /a x=1 > nul\r\ngoto loop\r\n",
    )
    .unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&cancel);
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        signal.store(true, Ordering::Release);
    });
    let started = Instant::now();
    let result = cache::export_cancellable(
        &stuck.to_string_lossy(),
        &source,
        600,
        &out,
        NetOptions::default(),
        &cancel,
        Duration::from_secs(5),
    );
    assert!(result.unwrap_err().contains("取消"));
    canceller.join().unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(std::fs::read(&out).unwrap(), b"original");
    assert!(cache::export_cancellable(
        &stuck.to_string_lossy(),
        &source,
        600,
        &out,
        NetOptions::default(),
        &AtomicBool::new(false),
        Duration::from_millis(100)
    )
    .unwrap_err()
    .contains("超时"));
    assert_eq!(std::fs::read(&out).unwrap(), b"original");
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        3,
        "不留下清单和半成品"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// 由包装脚本启动，故意继承并占住 stderr；只在专用测试环境内运行。
#[test]
#[ignore = "导出子进程 fixture，由 export_deadline_covers_inherited_stderr 单独启动"]
#[cfg(windows)]
fn export_pipe_holder_fixture() {
    if let Some(path) = std::env::var_os("XD_EXPORT_FIXTURE_PID") {
        std::fs::write(path, std::process::id().to_string()).unwrap();
        std::thread::sleep(Duration::from_secs(3));
    }
}

#[test]
#[ignore = "Windows 专用包装进程、继承 stderr 与进程树回收验证"]
#[cfg(windows)]
fn export_deadline_covers_inherited_stderr() {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn WaitForSingleObject(handle: *mut c_void, timeout: u32) -> u32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }
    for cancelled in [false, true] {
        let dir = tmp_dir(if cancelled {
            "pipe_cancel"
        } else {
            "pipe_timeout"
        });
        let source = dir.join("segments");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("fake.mkv"), b"synthetic").unwrap();
        let pid_file = dir.join("holder.pid");
        let wrapper = dir.join("wrapper.cmd");
        let executable = std::env::current_exe().unwrap();
        std::fs::write(&wrapper, format!(
            "@echo off\r\n:args\r\nif \"%~1\"==\"\" goto launch\r\nset \"output=%~1\"\r\nshift\r\ngoto args\r\n:launch\r\necho synthetic>\"%output%\"\r\nset \"XD_EXPORT_FIXTURE_PID={}\"\r\nstart \"\" /b \"{}\" --exact export_pipe_holder_fixture --ignored\r\n:ready\r\nif not exist \"{}\" goto ready\r\nexit /b 0\r\n",
            pid_file.display(), executable.display(), pid_file.display()
        )).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancel);
        let ready = pid_file.clone();
        let canceller = std::thread::spawn(move || {
            if cancelled {
                let deadline = Instant::now() + Duration::from_secs(2);
                while !ready.exists() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                // 让包装进程有机会退出，再在日志读取阶段取消。
                std::thread::sleep(Duration::from_millis(100));
                signal.store(true, Ordering::Release);
            }
        });
        let out = dir.join("keep.mkv");
        std::fs::write(&out, b"original").unwrap();
        let started = Instant::now();
        let result = cache::export_cancellable(
            wrapper.to_str().unwrap(),
            &source,
            600,
            &out,
            NetOptions::default(),
            &cancel,
            Duration::from_millis(if cancelled { 5000 } else { 500 }),
        );
        canceller.join().unwrap();
        let elapsed = started.elapsed();
        let pid: u32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
        unsafe {
            let process = OpenProcess(0x0010_0000, 0, pid);
            if !process.is_null() {
                let exited = WaitForSingleObject(process, 1000);
                CloseHandle(process);
                assert_eq!(exited, 0, "导出结束前必须回收继承 stderr 的子进程");
            }
        }
        assert!(result.is_err(), "不能在超时或取消后报告成功：{result:?}");
        assert!(result
            .unwrap_err()
            .contains(if cancelled { "取消" } else { "超时" }));
        assert!(
            elapsed < Duration::from_secs(2),
            "不能等待管道持有者自然退出：{elapsed:?}"
        );
        assert_eq!(std::fs::read(&out).unwrap(), b"original");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            4,
            "不留下清单和半成品"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
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
    assert!(picked.is_empty(), "窗口内没有片段时不能退回历史文件");

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
