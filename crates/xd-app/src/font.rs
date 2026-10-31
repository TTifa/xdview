//! 加载系统中文字体。
//!
//! egui 内置字体不含 CJK，中文会渲染成方块。这里从系统字体目录找一个中文字体注入
//! `FontDefinitions`。Windows 上常见的中文字体不少是 **TTC（字体集合）** 容器
//! （`msyh.ttc`、`simsun.ttc`），而字体加载器只吃单个 sfnt，因此这里自带一个
//! TTC → TTF 的抽取器：把第 `index` 个字体的表数据重新拼成一个独立字体文件。

use std::path::{Path, PathBuf};

pub struct LoadedFont {
    pub name: String,
    pub data: Vec<u8>,
}

const CANDIDATES: &[&str] = &[
    "msyh.ttc", // 微软雅黑（Win7+，中文 Windows 必备）
    "msyhbd.ttc",
    "simhei.ttf",  // 黑体
    "Deng.ttf",    // 等线
    "simsun.ttc",  // 宋体
    "simkai.ttf",  // 楷体
    "simfang.ttf", // 仿宋
    "NotoSansCJKsc-Regular.otf",
    "NotoSansSC-Regular.otf",
    "SourceHanSansSC-Regular.otf",
];

/// 找到第一个可用中文字体。
pub fn find_cjk_font() -> Option<LoadedFont> {
    let dir = font_dir()?;
    for name in CANDIDATES {
        let path = dir.join(name);
        if let Ok(bytes) = std::fs::read(&path) {
            if let Some(data) = to_single_font(bytes) {
                return Some(LoadedFont {
                    name: (*name).to_string(),
                    data,
                });
            }
        }
    }
    None
}

fn font_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".to_string());
        let dir = Path::new(&windir).join("Fonts");
        if dir.is_dir() {
            return Some(dir);
        }
        None
    }
    #[cfg(not(windows))]
    {
        for dir in ["/usr/share/fonts", "/usr/local/share/fonts"] {
            let p = Path::new(dir);
            if p.is_dir() {
                return Some(p.to_path_buf());
            }
        }
        None
    }
}

/// 统一成单个 sfnt 字体数据；无法识别返回 `None`。
fn to_single_font(data: Vec<u8>) -> Option<Vec<u8>> {
    if data.len() < 12 {
        return None;
    }
    let magic = [data[0], data[1], data[2], data[3]];
    if &magic == b"ttcf" {
        return extract_ttc_font(&data, 0);
    }
    // 0x00010000 = TrueType, 'OTTO' = CFF/OpenType, 'true'/'typ1' = 老式
    if magic == [0x00, 0x01, 0x00, 0x00]
        || &magic == b"OTTO"
        || &magic == b"true"
        || &magic == b"typ1"
    {
        return Some(data);
    }
    None
}

fn be16(d: &[u8], off: usize) -> Option<u16> {
    let b = d.get(off..off + 2)?;
    Some(u16::from_be_bytes([b[0], b[1]]))
}

fn be32(d: &[u8], off: usize) -> Option<u32> {
    let b = d.get(off..off + 4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// 从 TTC 中抽出第 `index` 个字体，返回独立 sfnt 字节。
pub fn extract_ttc_font(data: &[u8], index: usize) -> Option<Vec<u8>> {
    if data.len() < 12 || &data[0..4] != b"ttcf" {
        return None;
    }
    let num_fonts = be32(data, 8)? as usize;
    if index >= num_fonts || num_fonts == 0 {
        return None;
    }
    let sfnt_off = be32(data, 12 + index * 4)? as usize;
    let num_tables = be16(data, sfnt_off + 4)? as usize;
    if num_tables == 0 || num_tables > 512 {
        return None;
    }

    let header_len = 12 + num_tables * 16;
    let mut out = Vec::with_capacity(header_len * 2 + 64 * 1024);
    // sfnt 头 12 字节原样保留（version, numTables, searchRange, entrySelector, rangeShift）
    out.extend_from_slice(data.get(sfnt_off..sfnt_off + 12)?);

    // 先把表数据摆好位置，再回填每条记录
    let mut cursor = header_len;
    let mut records: Vec<[u8; 16]> = Vec::with_capacity(num_tables);
    let mut payload: Vec<u8> = Vec::new();

    for i in 0..num_tables {
        let rec = sfnt_off + 12 + i * 16;
        let mut record = [0u8; 16];
        record.copy_from_slice(data.get(rec..rec + 16)?);
        let table_off = u32::from_be_bytes([record[8], record[9], record[10], record[11]]) as usize;
        let table_len =
            u32::from_be_bytes([record[12], record[13], record[14], record[15]]) as usize;
        let table = data.get(table_off..table_off + table_len)?;

        let new_off = cursor as u32;
        record[8..12].copy_from_slice(&new_off.to_be_bytes());
        records.push(record);

        payload.extend_from_slice(table);
        let padded = (table_len + 3) & !3;
        payload.resize(payload.len() + (padded - table_len), 0);
        cursor += padded;
    }

    for r in records {
        out.extend_from_slice(&r);
    }
    out.extend_from_slice(&payload);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个最小可解析的 TTC：1 个字体、1 张表。
    fn fake_ttc() -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(b"ttcf");
        d.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        d.extend_from_slice(&1u32.to_be_bytes()); // numFonts
        d.extend_from_slice(&16u32.to_be_bytes()); // 第一个字体偏移

        // sfnt 头
        d.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        d.extend_from_slice(&1u16.to_be_bytes()); // numTables
        d.extend_from_slice(&0u16.to_be_bytes());
        d.extend_from_slice(&0u16.to_be_bytes());
        d.extend_from_slice(&0u16.to_be_bytes());

        // 表记录：tag 'head'，checksum 0，offset 44（紧跟记录之后），length 4
        d.extend_from_slice(b"head");
        d.extend_from_slice(&0u32.to_be_bytes());
        d.extend_from_slice(&44u32.to_be_bytes());
        d.extend_from_slice(&4u32.to_be_bytes());

        // 表数据
        d.extend_from_slice(&[1, 2, 3, 4]);
        d
    }

    #[test]
    fn extracts_single_font_from_ttc() {
        let ttc = fake_ttc();
        let sfnt = extract_ttc_font(&ttc, 0).expect("应能抽取");
        assert_eq!(&sfnt[0..4], &[0, 1, 0, 0]);
        assert_eq!(be16(&sfnt, 4).unwrap(), 1);
        // 新偏移应为 12 + 16 = 28
        assert_eq!(be32(&sfnt, 12 + 8).unwrap(), 28);
        assert_eq!(&sfnt[28..32], &[1, 2, 3, 4]);
    }

    #[test]
    fn rejects_non_ttc_and_bad_index() {
        assert!(extract_ttc_font(b"not a font", 0).is_none());
        let ttc = fake_ttc();
        assert!(extract_ttc_font(&ttc, 5).is_none());
    }

    #[test]
    fn single_sfnt_passes_through() {
        let mut d = vec![0u8, 1, 0, 0];
        d.extend_from_slice(&[0u8; 32]);
        assert!(to_single_font(d.clone()).is_some());
        assert!(to_single_font(vec![0xFFu8; 40]).is_none());
    }

    /// 拿本机真实中文字体验证抽取结果结构完整（无字体则跳过）。
    #[test]
    fn system_font_is_structurally_valid() {
        let Some(f) = find_cjk_font() else {
            eprintln!("跳过：本机未找到中文字体");
            return;
        };
        let d = &f.data;
        assert!(d.len() > 4096, "字体太小，可疑: {} 字节", d.len());
        let magic = [d[0], d[1], d[2], d[3]];
        assert!(
            magic == [0, 1, 0, 0] || &magic == b"OTTO",
            "抽取结果不是合法 sfnt: {:?}",
            f.name
        );
        let num_tables = u16::from_be_bytes([d[4], d[5]]) as usize;
        assert!((1..=512).contains(&num_tables), "表数量异常: {num_tables}");
        for i in 0..num_tables {
            let rec = 12 + i * 16;
            let off =
                u32::from_be_bytes([d[rec + 8], d[rec + 9], d[rec + 10], d[rec + 11]]) as usize;
            let len =
                u32::from_be_bytes([d[rec + 12], d[rec + 13], d[rec + 14], d[rec + 15]]) as usize;
            assert!(off + len <= d.len(), "第 {i} 张表越界: off={off} len={len}");
        }
        eprintln!("字体 {} -> {} 字节, {} 张表", f.name, d.len(), num_tables);
    }
}
