//! xd-media —— 拉流与解码。
//!
//! # 为什么用 ffmpeg 子进程而不是在 Rust 里解码
//!
//! 媒体传输、解复用和编解码复用 ffmpeg，Rust 管理参数、帧、重连与进程生命周期。
//! 支持的输入格式取决于本机 ffmpeg 构建，不在 Rust 中实现 H.26x 解码器。
//!
//! 代价是多一个进程与一次 RGBA 拷贝（1080p 约 8MB/帧），对本场景可接受。
//!
//! # 管线
//!
//! ```text
//! ffprobe ──► 分辨率/编码 ──► 计算目标尺寸
//!                              │
//!         ffmpeg -i url -f rawvideo -pix_fmt rgba -   （stdout）
//!                              │
//!              读线程 read_exact(w*h*4) ──► Frame ──► mpsc ──► UI
//! ```

pub mod cache;
pub mod ffmpeg;
pub mod frame;
pub mod record;
pub mod stream;
pub mod sys;

pub use cache::{CacheOptions, CacheUsage, SegmentCache};
pub use ffmpeg::{NetOptions, ProbeInfo};
pub use frame::Frame;
pub use record::Recorder;
pub use stream::{Event, Stream, StreamOptions};
