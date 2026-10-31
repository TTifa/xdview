//! 拉流线程：探测 → 启动 ffmpeg → 逐帧读出 → 断流自动重连。

use crate::ffmpeg::{self, apply_net, decode_args, NetOptions};
use crate::frame::Frame;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 入流瞬间（还没等到 IDR）必然出现、但对使用无影响的解码器抱怨。
const BENIGN_DECODER_NOISE: &[&str] = &[
    "Could not find ref",
    "Skipping invalid undecodable NALU",
    "Error constructing the frame RPS",
    "non monotonically increasing dts",
];

#[derive(Debug, Clone)]
pub enum Event {
    /// 连接建立，已知解码尺寸
    Opened {
        width: usize,
        height: usize,
        codec: String,
    },
    Frame(Frame),
    /// 过程信息（ffmpeg stderr、重连提示）
    Status(String),
    /// 本次连接失败，即将重试（或不再重试）
    Failed(String),
    /// 线程结束
    Closed,
}

#[derive(Debug, Clone)]
pub struct StreamOptions {
    pub url: String,
    pub ffmpeg: String,
    pub max_width: u32,
    pub low_latency: bool,
    pub reconnect: bool,
    pub net: NetOptions,
}

impl StreamOptions {
    pub fn new(url: impl Into<String>, ffmpeg: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ffmpeg: ffmpeg.into(),
            max_width: 1280,
            low_latency: true,
            reconnect: true,
            net: NetOptions::default(),
        }
    }
}

pub struct Stream {
    rx: Receiver<Event>,
    stop: Arc<AtomicBool>,
    killer: Arc<Mutex<Option<Child>>>,
    handle: Option<JoinHandle<()>>,
}

impl Stream {
    pub fn start(opts: StreamOptions) -> Stream {
        let (tx, rx) = sync_channel(4);
        let stop = Arc::new(AtomicBool::new(false));
        let killer: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));

        let handle = {
            let stop = Arc::clone(&stop);
            let killer = Arc::clone(&killer);
            thread::spawn(move || supervisor(opts, tx, stop, killer))
        };

        Stream {
            rx,
            stop,
            killer,
            handle: Some(handle),
        }
    }

    pub fn try_recv(&self) -> Option<Event> {
        self.rx.try_recv().ok()
    }

    pub fn recv_timeout(&self, d: Duration) -> Option<Event> {
        self.rx.recv_timeout(d).ok()
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(mut child) = self.killer.lock().ok().and_then(|mut g| g.take()) {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop();
    }
}

fn supervisor(
    opts: StreamOptions,
    tx: SyncSender<Event>,
    stop: Arc<AtomicBool>,
    killer: Arc<Mutex<Option<Child>>>,
) {
    let seq = AtomicU64::new(0);
    let mut backoff = Duration::from_millis(500);
    let mut first = true;

    while !stop.load(Ordering::SeqCst) {
        if !first {
            if !sleep_interruptible(&stop, backoff) {
                break;
            }
            backoff = (backoff * 2).min(Duration::from_secs(8));
        }
        first = false;

        // 传入 stop：停止拉流时探测立刻放弃并杀掉 ffprobe，不用干等满 8 秒
        let probe = ffmpeg::probe(&opts.ffmpeg, &opts.url, opts.net, &stop);
        if probe.is_none() {
            status(&tx, "ffprobe 未取到分辨率，按 1280x720 兜底".to_string());
        }
        let (w, h, need_scale) = ffmpeg::target_size(probe.as_ref(), opts.max_width);
        let codec = probe
            .as_ref()
            .map(|p| p.codec.clone())
            .unwrap_or_else(|| "unknown".to_string());
        if let Some(p) = probe.as_ref() {
            if let Some(fps) = p.fps {
                status(
                    &tx,
                    format!("源 {}x{} {} @{:.1}fps", p.width, p.height, p.codec, fps),
                );
            }
        }

        let args = decode_args(&opts.url, opts.low_latency, w, h, need_scale, opts.net);
        let mut cmd = Command::new(&opts.ffmpeg);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_net(&mut cmd, opts.net);

        // spawn_managed 统一负责隐藏窗口与作业对象归属：
        // 少了后者，父进程被强杀时 ffmpeg 会变成孤儿一直往磁盘写。
        let mut child = match crate::sys::spawn_managed(&mut cmd) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!(
                    "启动 ffmpeg 失败（{}）：{e}。请确认已安装 ffmpeg 或在配置里写全路径",
                    opts.ffmpeg
                );
                let _ = tx.try_send(Event::Failed(msg));
                if !opts.reconnect {
                    break;
                }
                continue;
            }
        };

        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => {
                let _ = tx.try_send(Event::Failed("ffmpeg 没有 stdout".into()));
                break;
            }
        };
        if let Some(err) = child.stderr.take() {
            let tx_err = tx.clone();
            thread::spawn(move || {
                let reader = BufReader::new(err);
                for line in reader.lines().map_while(Result::ok) {
                    // ffmpeg 会整行回显带账号密码/签名的地址，进日志面板与截图前先脱敏
                    let line = xd_core::onvif::mask_url_in_text(line.trim());
                    if line.is_empty() {
                        continue;
                    }
                    // 起播时从中途 GOP 接入必然报这些，属正常现象，不进日志面板
                    if BENIGN_DECODER_NOISE.iter().any(|p| line.contains(p)) {
                        continue;
                    }
                    match tx_err.try_send(Event::Status(line)) {
                        Ok(_) => {}
                        Err(TrySendError::Full(_)) => {}
                        Err(TrySendError::Disconnected(_)) => break,
                    }
                }
            });
        }

        if let Ok(mut slot) = killer.lock() {
            *slot = Some(child);
        }
        if !tx
            .try_send(Event::Opened {
                width: w,
                height: h,
                codec,
            })
            .is_ok()
        {
            // 接收端已关闭
            kill_child(&killer);
            break;
        }

        let frame_len = Frame::byte_len(w, h);
        let mut buf = vec![0u8; frame_len];
        let mut reader = BufReader::with_capacity(1024 * 1024, stdout);
        let mut got_any = false;

        loop {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            match reader.read_exact(&mut buf) {
                Ok(()) => {
                    got_any = true;
                    let frame = Frame {
                        width: w,
                        height: h,
                        rgba: buf.clone(),
                        seq: seq.fetch_add(1, Ordering::Relaxed),
                        captured_ms: now_ms(),
                    };
                    match tx.try_send(Event::Frame(frame)) {
                        Ok(_) => {}
                        // 满了就丢帧：宁可丢帧也不要堆积延迟
                        Err(TrySendError::Full(_)) => {}
                        Err(TrySendError::Disconnected(_)) => {
                            kill_child(&killer);
                            return;
                        }
                    }
                }
                Err(_) => break,
            }
        }

        kill_child(&killer);
        if stop.load(Ordering::SeqCst) {
            break;
        }

        if got_any {
            backoff = Duration::from_millis(500);
            status(&tx, "流中断，重连中…".to_string());
        } else {
            let _ = tx.try_send(Event::Failed("未取到任何视频帧".into()));
        }
        if !opts.reconnect {
            break;
        }
    }

    let _ = tx.try_send(Event::Closed);
}

fn kill_child(killer: &Arc<Mutex<Option<Child>>>) {
    if let Ok(mut slot) = killer.lock() {
        if let Some(mut child) = slot.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn status(tx: &SyncSender<Event>, msg: String) {
    let _ = tx.try_send(Event::Status(msg));
}

fn sleep_interruptible(stop: &Arc<AtomicBool>, total: Duration) -> bool {
    let step = Duration::from_millis(100);
    let mut slept = Duration::ZERO;
    while slept < total {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        thread::sleep(step);
        slept += step;
    }
    !stop.load(Ordering::SeqCst)
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
