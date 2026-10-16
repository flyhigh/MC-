//! MC 壁纸 · 位图读取与缩放 —— 「生图返回的字节」与「GPU 纹理 / 面板缩略图 / 调色板采样」
//! 之间的那一层。
//!
//! 为什么单开一个 crate,而不是把这几行塞进调用方:
//! - **解码只做一次**。同一张图要喂三处:渲染器当纹理(要全分辨率 RGBA)、面板当缩略图(要小)、
//!   调色板提取要采样。三处各自 `image::open` 的话,一次生图要解三遍 —— 1080p 就是
//!   3 × 8 MB 的白干,而且全在用户等结果那条路上。
//! - `mcwp-render` 不该认识 PNG:渲染器只吃 RGBA 字节,把 `image` 拖进渲染 crate 会让
//!   壁纸进程的依赖树凭空多出一个解码器(而它其实只在"生图 / 换图"那一瞬间用得上)。
//! - 放进 `mcwp-spec` 更不行 —— 那是个只有 `serde` 的"契约"crate,连 `serde_json` 都算多余。

use std::path::Path;

use anyhow::{Context, Result};

/// 一张解好的 8 位 RGBA 位图,行优先、无 padding。
///
/// 直接就是 wgpu `write_texture` 要的形状,也是 `slint::SharedPixelBuffer` 要的形状 ——
/// 中间不再有第二次拷贝的理由。
#[derive(Clone, PartialEq, Eq)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    /// 长度恒等于 `width * height * 4`
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for Bitmap {
    /// **故意不打像素**:8 MB 的 `Vec<u8>` 按默认实现会往日志里刷几百万个数字。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bitmap")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.rgba.len())
            .finish()
    }
}

impl Bitmap {
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0 || self.rgba.len() < 4
    }

    /// 从 PNG / JPEG 字节解码。格式按文件头猜,不认扩展名(生图服务商给的 URL 常常没有后缀)。
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            anyhow::bail!("这张图是空的(0 字节)");
        }
        let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .context("看不出这是什么图片格式")?;
        let format = reader.format();
        let decoded = reader
            .decode()
            .with_context(|| format!("解码失败(识别成 {format:?})"))?;
        let rgba = decoded.to_rgba8();
        Ok(Self {
            width: rgba.width(),
            height: rgba.height(),
            rgba: rgba.into_raw(),
        })
    }

    /// 从磁盘读一张图。扫壁纸库和面板出缩略图都走这里。
    pub fn decode_file(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("读图失败:{}", path.display()))?;
        Self::decode(&bytes).with_context(|| format!("解析图失败:{}", path.display()))
    }

    /// 等比缩到不超过 `max_width × max_height`。**永不放大** —— 小图放大只会更糊,
    /// 而且多出来的像素全要显存。
    pub fn thumbnail(&self, max_width: u32, max_height: u32) -> Self {
        if self.is_empty() {
            return self.clone();
        }
        let (width, height) = fit(self.width, self.height, max_width, max_height);
        if width == self.width && height == self.height {
            return self.clone();
        }
        let Some(source) = image::RgbaImage::from_raw(self.width, self.height, self.rgba.clone())
        else {
            return self.clone();
        };
        // `Triangle` 足够了:缩略图最大也就一两百像素宽,`Lanczos3` 在这里买到的那点锐度
        // 在 40×24 的卡片上看不出来,却要让每次刷新库多花几十毫秒。
        let scaled = image::imageops::resize(&source, width, height, image::imageops::FilterType::Triangle);
        Self {
            width,
            height,
            rgba: scaled.into_raw(),
        }
    }

    /// 编成 PNG 字节。
    ///
    /// **只给缩略图用**:原图永远直接落盘、绝不重编码(重压一次只会让画面更糊,还白花一次
    /// CPU —— 见 `mcwp-wallpaper/src/photos.rs` 的 `save`)。缩略图则是我们自己缩出来的,
    /// 本来就没有"原始字节"可留。
    pub fn encode_png(&self) -> Result<Vec<u8>> {
        let image = image::RgbaImage::from_raw(self.width, self.height, self.rgba.clone())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "像素数和尺寸对不上:{}×{} 要 {} 字节,实际 {}",
                    self.width,
                    self.height,
                    self.width as usize * self.height as usize * 4,
                    self.rgba.len()
                )
            })?;
        let mut out = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut out, image::ImageFormat::Png)
            .context("编 PNG 失败")?;
        Ok(out.into_inner())
    }
}

/// 等比缩放的落点:长边贴到限制,短边按比例走,且**不放大**。
pub fn fit(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (width, height);
    }
    let scale = (max_width as f64 / width as f64).min(max_height as f64 / height as f64).min(1.0);
    let scaled_width = ((width as f64 * scale).round() as u32).max(1);
    let scaled_height = ((height as f64 * scale).round() as u32).max(1);
    (scaled_width, scaled_height)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一张纯色 PNG,当作"生图返回的字节"。
    fn solid_png(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
        let mut image = image::RgbImage::new(width, height);
        for pixel in image.pixels_mut() {
            *pixel = image::Rgb(rgb);
        }
        let mut out = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("写 PNG 不该失败");
        out.into_inner()
    }

    #[test]
    fn 解码出正方形尺寸与像素() {
        let bytes = solid_png(4, 2, [10, 20, 30]);
        let bitmap = Bitmap::decode(&bytes).expect("该能解码");
        assert_eq!((bitmap.width, bitmap.height), (4, 2));
        assert_eq!(bitmap.rgba.len(), 4 * 2 * 4);
        assert_eq!(&bitmap.rgba[0..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn 缩略图保比例且不放大() {
        let bytes = solid_png(1920, 1080, [200, 100, 50]);
        let bitmap = Bitmap::decode(&bytes).unwrap();
        let small = bitmap.thumbnail(160, 160);
        assert_eq!((small.width, small.height), (160, 90));
        assert_eq!(small.rgba.len(), 160 * 90 * 4);

        // 已经比限制小的图原样返回,不能放大
        let tiny = bitmap.thumbnail(4000, 4000);
        assert_eq!((tiny.width, tiny.height), (1920, 1080));
    }

    #[test]
    fn 空字节与垃圾字节都只是报错不是崩溃() {
        assert!(Bitmap::decode(&[]).is_err());
        assert!(Bitmap::decode(b"not an image at all").is_err());
    }
}
