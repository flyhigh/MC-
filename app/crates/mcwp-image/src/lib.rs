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

    /// 按 `target_width : target_height` 的**精确比例**居中裁一刀:只丢边,不缩放、不重采样。
    ///
    /// 为什么非得"精确":图生视频的服务商拿"短边压到 1080 × 图片比例、向下取整"去推成片,
    /// 比例差一个像素就可能把长边推回奇数,而奇数它编不出来、还一个字都不解释(见
    /// `mcwp_ai::VIDEO_SIZES`)。所以这里先把目标比例约到最简再取**整数倍** ——
    /// `16:9` 只会裁出 `16k × 9k`,推回去恰好是 1920(偶数)。
    ///
    /// 裁不出一个整数倍(图小得离谱)时原样返回:宁可让它按老样子失败,也不要凭空造出一张
    /// 1×1 的图去提交。
    pub fn crop_to_ratio(&self, target_width: u32, target_height: u32) -> Self {
        let (width, height) = ratio_box(self.width, self.height, target_width, target_height);
        if width == self.width && height == self.height {
            return self.clone();
        }
        let x = (self.width - width) / 2;
        let y = (self.height - height) / 2;
        let row_bytes = (width * 4) as usize;
        let mut rgba = Vec::with_capacity(row_bytes * height as usize);
        for row in y..y + height {
            let start = ((row * self.width + x) * 4) as usize;
            rgba.extend_from_slice(&self.rgba[start..start + row_bytes]);
        }
        Self {
            width,
            height,
            rgba,
        }
    }

    /// 编成 JPEG(质量 0-100)。
    ///
    /// **只给"提交前必须改尺寸"那一段用**(见 `crop_to_ratio`):图已经被裁过、没有"原始
    /// 字节"可留了,只能重编一次。用 JPEG 而不是 PNG,是因为它要内联进 base64 过网络 ——
    /// 同样一张 1920×1080,PNG 能到几 MB,JPEG 只要几百 KB。
    pub fn encode_jpeg(&self, quality: u8) -> Result<Vec<u8>> {
        let rgba = image::RgbaImage::from_raw(self.width, self.height, self.rgba.clone())
            .ok_or_else(|| anyhow::anyhow!("像素数和尺寸对不上:{}×{}", self.width, self.height))?;
        // JPEG 没有 alpha 通道,先落到 RGB
        let rgb = image::DynamicImage::ImageRgba8(rgba).to_rgb8();
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality)
            .encode_image(&rgb)
            .context("编 JPEG 失败")?;
        Ok(out)
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

/// 落在 `width × height` 里、比例**精确等于** `target_width : target_height` 的最大居中框。
///
/// "精确"的代价是只能取整数倍(`16:9` → `16k × 9k`),换来的是推回去的长边一定落在整数上
/// —— 见 `crop_to_ratio`。
pub fn ratio_box(width: u32, height: u32, target_width: u32, target_height: u32) -> (u32, u32) {
    if width == 0 || height == 0 || target_width == 0 || target_height == 0 {
        return (width, height);
    }
    let divisor = gcd(target_width, target_height).max(1);
    let (unit_width, unit_height) = (target_width / divisor, target_height / divisor);
    let k = (width / unit_width).min(height / unit_height);
    if k == 0 {
        return (width, height);
    }
    (unit_width * k, unit_height * k)
}

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let next = a % b;
        a = b;
        b = next;
    }
    a
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

    /// 裁出来的框必须是**精确**比例:服务商是拿它去推长边的,差一个像素就可能推回奇数。
    #[test]
    fn 按比例裁完是精确比例且只丢边() {
        let bitmap = Bitmap::decode(&solid_png(1920, 1088, [120, 90, 60])).unwrap();
        let cropped = bitmap.crop_to_ratio(1920, 1080);
        assert_eq!((cropped.width, cropped.height), (1920, 1080));
        assert_eq!(cropped.rgba.len(), 1920 * 1080 * 4);
        // 竖屏:16:9 换成 9:16
        let portrait = Bitmap::decode(&solid_png(1088, 1920, [10, 20, 30])).unwrap();
        let cropped = portrait.crop_to_ratio(1080, 1920);
        assert_eq!((cropped.width, cropped.height), (1080, 1920));
        // 比例对不上的时候取"整数倍":1808×1136 裁到 16:9 是 1808×1017(= 113 × 16 / 9)
        let odd = Bitmap::decode(&solid_png(1808, 1136, [1, 2, 3])).unwrap();
        let cropped = odd.crop_to_ratio(1920, 1080);
        assert_eq!((cropped.width, cropped.height), (1808, 1017));
        assert_eq!(cropped.width as u64 * 1080, cropped.height as u64 * 1920);
        // 比例已经对了就原样返回(不白搬一遍像素)
        let same = bitmap.crop_to_ratio(1920, 1088);
        assert_eq!((same.width, same.height), (1920, 1088));
    }

    /// 编出来的字节得能被当成 JPEG(服务商只认 PNG / JPEG,见 `image_data_url`)。
    #[test]
    fn 裁完能编成_jpeg() {
        let bitmap = Bitmap::decode(&solid_png(64, 36, [200, 30, 40])).unwrap();
        let bytes = bitmap.encode_jpeg(92).expect("该能编");
        assert_eq!(&bytes[0..3], &[0xFF, 0xD8, 0xFF]);
        let back = Bitmap::decode(&bytes).expect("自己编的自己得解得开");
        assert_eq!((back.width, back.height), (64, 36));
    }

    #[test]
    fn 空字节与垃圾字节都只是报错不是崩溃() {
        assert!(Bitmap::decode(&[]).is_err());
        assert!(Bitmap::decode(b"not an image at all").is_err());
    }
}
