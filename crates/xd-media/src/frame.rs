//! 一帧解码后的画面（RGBA8，未预乘）。

#[derive(Clone)]
pub struct Frame {
    pub width: usize,
    pub height: usize,
    /// 长度恒为 `width * height * 4`，字节序 R,G,B,A。
    pub rgba: Vec<u8>,
    /// 自进程启动以来的递增序号，用于统计丢帧。
    pub seq: u64,
    /// 读到这一帧时的 UNIX 毫秒时间戳。
    pub captured_ms: u128,
}

impl Frame {
    pub fn byte_len(width: usize, height: usize) -> usize {
        width * height * 4
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.rgba
    }

    /// 拷出为 BMP（32 位，自底向上）。零依赖、Windows 双击可直接打开。
    pub fn to_bmp(&self) -> Vec<u8> {
        let row_bytes = self.width * 4;
        let pixel_bytes = row_bytes * self.height;
        let file_size = 14 + 40 + pixel_bytes;
        let mut out = Vec::with_capacity(file_size);

        // BITMAPFILEHEADER
        out.extend_from_slice(b"BM");
        out.extend_from_slice(&(file_size as u32).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(54u32).to_le_bytes());

        // BITMAPINFOHEADER
        out.extend_from_slice(&(40u32).to_le_bytes());
        out.extend_from_slice(&(self.width as i32).to_le_bytes());
        out.extend_from_slice(&(self.height as i32).to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        out.extend_from_slice(&(pixel_bytes as u32).to_le_bytes());
        out.extend_from_slice(&2835i32.to_le_bytes());
        out.extend_from_slice(&2835i32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());

        // 像素：BMP 是 BGRA 且自底向上
        for y in (0..self.height).rev() {
            let row = &self.rgba[y * row_bytes..(y + 1) * row_bytes];
            for px in row.chunks_exact(4) {
                out.push(px[2]);
                out.push(px[1]);
                out.push(px[0]);
                out.push(255);
            }
        }
        out
    }
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.rgba.len())
            .field("seq", &self.seq)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: usize, h: usize, rgba: [u8; 4]) -> Frame {
        let mut data = Vec::with_capacity(w * h * 4);
        for _ in 0..w * h {
            data.extend_from_slice(&rgba);
        }
        Frame {
            width: w,
            height: h,
            rgba: data,
            seq: 0,
            captured_ms: 0,
        }
    }

    #[test]
    fn bmp_header_is_consistent() {
        let f = solid(2, 3, [10, 20, 30, 255]);
        let bmp = f.to_bmp();
        assert_eq!(&bmp[0..2], b"BM");
        assert_eq!(bmp.len(), 54 + 2 * 3 * 4);
        let file_size = u32::from_le_bytes([bmp[2], bmp[3], bmp[4], bmp[5]]);
        assert_eq!(file_size as usize, bmp.len());
        // 宽高
        let w = i32::from_le_bytes([bmp[18], bmp[19], bmp[20], bmp[21]]);
        let h = i32::from_le_bytes([bmp[22], bmp[23], bmp[24], bmp[25]]);
        assert_eq!((w, h), (2, 3));
        // 第一个像素应为 BGR(A)
        assert_eq!(&bmp[54..58], &[30, 20, 10, 255]);
    }

    #[test]
    fn bmp_rows_are_bottom_up() {
        let mut f = solid(1, 2, [0, 0, 0, 255]);
        // 第 0 行设为白，第 1 行设为黑
        f.rgba[0..4].copy_from_slice(&[255, 255, 255, 255]);
        let bmp = f.to_bmp();
        // 文件里第一行像素来自图像最后一行（黑）
        assert_eq!(&bmp[54..58], &[0, 0, 0, 255]);
        assert_eq!(&bmp[58..62], &[255, 255, 255, 255]);
    }
}
