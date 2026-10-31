//! 把滚动缓存里最近 N 分钟的片段拼成一个文件（`-c copy`，不重编码）。
//!
//! ```text
//! cargo run -p xd-media --example export_cache -- cache/tplink 10 exports/out.mkv
//! ```

use std::path::PathBuf;

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        return Err("用法: export_cache <缓存目录> <最近几分钟> <输出文件>".to_string());
    }
    let dir = PathBuf::from(&args[0]);
    let minutes: u64 = args[1].parse().map_err(|_| "分钟数不是整数".to_string())?;
    let out = PathBuf::from(&args[2]);

    let files = xd_media::cache::list_segments(&dir);
    println!("缓存目录: {}", dir.display());
    println!("片段数量: {}", files.len());
    let total: u64 = files.iter().map(|(_, size, _)| *size).sum();
    println!("占用: {:.1} MB", total as f64 / 1_048_576.0);

    let n = xd_media::cache::export(
        "ffmpeg",
        &dir,
        minutes * 60,
        &out,
        xd_media::NetOptions::default(),
    )?;
    if n == 0 {
        println!("没有可导出的片段（目录为空，或时间窗内没内容）");
        return Ok(());
    }
    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    println!(
        "已导出 {n} 个片段 -> {}（{:.1} MB）",
        out.display(),
        size as f64 / 1_048_576.0
    );
    Ok(())
}
