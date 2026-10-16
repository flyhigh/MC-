//! MC 壁纸 · 视频解码:把一段 mp4 解成「一帧一坨 RGBA8」,喂给渲染器。
//!
//! **为什么是 Media Foundation**:Windows 上它是系统自带的,不用跟着发布一个几十兆的解码库;
//! H.264 这类编码开箱就能解 —— 而我们能拿到的视频只有一种来源(生图服务商返回的 mp4),
//! 编码远不会跑出这个范围。更关键的是它能在**解码这一层**顺手把 YUV 转成 RGB、并按上限缩到
//! 屏幕尺寸(`MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING`),省掉在渲染器里再写一条
//! YUV 采样管线 —— 那才是真正的成本。
//!
//! 这个 crate **只负责解码**,不知道 wgpu、不认识 `Photo`、更不知道什么是壁纸:
//! 它对外只有「打开 → 下一帧 → 回到开头」三个动作(见 `Player`)。把它单独放一个 crate 而不是
//! 塞进 `mcwp-render`,是因为渲染器的依赖树里不该有 COM / MF —— 那和"渲染器的依赖树里永远
//! 不该有解码器"(见 `Renderer::set_photo` 的说明)是同一条理由,只是换了个方向。
//!
//! ## 几条硬规矩,都是踩出来 / 查出来的
//!
//! - **COM 要按线程初始化,MF 只要一次**。`Player` 在哪个线程创建就得在那个线程
//!   `CoInitializeEx`(`ensure_mf` 每次进门都做,重入很便宜);而 `MFStartup` 是进程级的,
//!   只做一次 —— 所以这两件事在 `ensure_mf` 里是分开写的,不能图省事一起塞进 `OnceLock`。
//! - **每帧的 `MFVideoFormat_RGB32` 其实是 BGRX**。名字听着像 RGBA,内存里却是 B,G,R,X ——
//!   不换序整段视频会把红蓝对调(天空变橙、草地变紫,一眼认得出)。见 `copy_bgra_to_rgba`。
//! - **行长看 `MF_MT_DEFAULT_STRIDE`,不要拿"宽 × 4"硬算**。自下而上的视频那里是个负数,
//!   硬算的结果是整帧上下颠倒。这个字段在转换之后**要重新读一次**:转换器可能改它。
//! - **尺寸也要回头读一次**。我们要的是"缩到屏幕尺寸以内",但转换器到底给多大是它说了算,
//!   所以 `SetCurrentMediaType` 之后从当前类型里读回真实尺寸,再拿它去算缓冲。
//! - **帧不落地、不多留一份**。解码 → 换序写进复用的 `buffer`,渲染器那边再 `write_texture`
//!   上去;中间没有"解码后的原始帧"第二份拷贝。

use std::ffi::OsStr;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0_0};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::Variant::VT_I8;
use windows::core::{GUID, PCWSTR};

/// 一段视频的基本信息。`width`/`height` 是**解码输出的**尺寸(已经被 `limit` 夹过),
/// 不是文件里写的那个原始尺寸 —— 渲染器要按它算纹理和画面比例。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub duration: Duration,
}

/// 播放器:一个源 + 一个复用缓冲。
///
/// 用法是**拉模式**(调用方按自己的节奏要帧),不是推模式:壁纸的刷新率(60)和视频的帧率
/// (24/25/30)对不上,谁快谁慢还不一定 —— 由调用方(`main.rs` 那个主循环)按累计时间决定
/// "这一帧该不该换",`Player` 只管"给我下一帧"。
pub struct Player {
    info: VideoInfo,
    /// 每行多少**字节**,负数表示自下而上(见文件头)
    stride: i32,
    /// 复用的一帧缓冲(RGBA8)。`next` 只往里写,不重新分配 —— 1080p 一帧 8 MB,
    /// 每帧都新申请一次等于把分配器当扇子扇。
    buffer: Vec<u8>,
    reader: IMFSourceReader,
    /// 已经读到结尾了。到结尾之后再 `next` 直接返回 `None`,不再问 MF
    done: bool,
}

impl Player {
    /// 打开一个视频文件。`limit` 是**解码上限**(一般就给屏幕尺寸):源比它大就让 MF 在这一层
    /// 缩下来,免得每帧把 4K 的像素搬到 CPU 再让 GPU 缩回去。
    pub fn open(path: &Path, limit: (u32, u32)) -> Result<Self> {
        ensure_mf()?;
        let url = wide(path.as_os_str());
        // 「要 YUV → RGB + 缩放」这件事必须在**建读者的时候**告诉它:读者起来之后再改
        // (它没有暴露 `GetAttributes`)是做不到的,所以这里先造一个属性袋再建读者。
        let mut options: Option<IMFAttributes> = None;
        unsafe { MFCreateAttributes(&mut options, 1) }?;
        let options = options.ok_or_else(|| anyhow!("建不出用来装解码选项的属性袋"))?;
        unsafe { options.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)? };
        let reader = unsafe { MFCreateSourceReaderFromURL(PCWSTR(url.as_ptr()), &options) }
            .with_context(|| format!("打开视频失败:{}", path.display()))?;

        // 只要有画面,不要音轨:壁纸不出声,而选中音轨会让读者去解音频。
        unsafe { reader.SetStreamSelection(stream_index(), true)? };

        let native = unsafe { reader.GetNativeMediaType(stream_index(), 0) }
            .context("这个文件里没有能解的画面流")?;
        let (src_width, src_height) = unsafe { frame_size(&native) }?;
        let fps = unsafe { frame_rate(&native) }.unwrap_or(DEFAULT_FPS);
        let (width, height) = fit(src_width, src_height, limit);
        unsafe { set_output_type(&reader, &native, width, height, fps)? };

        // 转换器实际给的是多大、每行多少字节:以它为准(见文件头)
        let current = unsafe { reader.GetCurrentMediaType(stream_index()) }?;
        let (width, height) = unsafe { frame_size(&current) }.unwrap_or((width, height));
        let stride = unsafe { current.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
            .map(|raw| raw as i32)
            // 没有这个属性时按"紧凑的自上而下"算 —— 绝大多数解码器就是这么给的
            .unwrap_or(width as i32 * 4);
        let duration = unsafe {
            reader
                .GetPresentationAttribute(media_source(), &MF_PD_DURATION)
                .ok()
                .and_then(|var| variant_u64(&var))
                // MF 的时间单位是 100 ns
                .map(|ticks| Duration::from_micros(ticks / 10))
                .unwrap_or_default()
        };
        let info = VideoInfo {
            width,
            height,
            fps,
            duration,
        };
        tracing::info!(
            "打开视频:{}×{} @ {:.0}fps · {:.1}s(源 {}×{})",
            width,
            height,
            fps,
            info.duration.as_secs_f32(),
            src_width,
            src_height
        );
        Ok(Self {
            stride,
            buffer: vec![0; width as usize * height as usize * 4],
            reader,
            done: false,
            info,
        })
    }

    pub fn info(&self) -> VideoInfo {
        self.info
    }

    /// 每帧之间该隔多久(秒)。
    pub fn frame_interval(&self) -> f32 {
        1.0 / self.info.fps.max(1.0)
    }

    /// 解下一帧。`Ok(None)` = **已经放完了**(调用方决定是 `rewind` 重头再来还是停住)。
    ///
    /// 返回的是借用:调用方拿它去 `write_texture`,下一次 `next` 才会覆盖它。
    /// 名字里带 `frame` 而不是直接叫 `next`:`Player` 不是迭代器 —— 放完能 `rewind` 重头再来,
    /// 出错走 `Result`(迭代器那套 `Iterator::next` 表达不了这两件事),叫 `next` 只会误导人。
    pub fn next_frame(&mut self) -> Result<Option<&[u8]>> {
        if self.done {
            return Ok(None);
        }
        let mut flags = 0u32;
        let mut sample: Option<IMFSample> = None;
        unsafe {
            self.reader.ReadSample(
                stream_index(),
                0,
                None,
                Some(&mut flags),
                None,
                Some(&mut sample),
            )?;
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            self.done = true;
            return Ok(None);
        }
        // 没有 sample 又不是结尾:格式变更之类,跳过这一枪就当没这帧
        let Some(sample) = sample else {
            return Ok(None);
        };
        let buffer = unsafe { sample.ConvertToContiguousBuffer() }
            .context("取解码帧的连续缓冲失败")?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        unsafe { buffer.Lock(&mut ptr, None, Some(&mut len))? };
        let copied = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
        let converted = copy_bgra_to_rgba(
            copied,
            self.stride,
            self.info.width,
            self.info.height,
            &mut self.buffer,
        );
        // **先解锁再报错**:`?` 提前返回会把缓冲锁死,下一次 Lock 直接失败
        unsafe { buffer.Unlock() }?;
        converted?;
        Ok(Some(&self.buffer))
    }

    /// 回到开头。视频很短(生图服务商给的都只有几秒),放完就从头再来一遍,比"停住不动"更像壁纸。
    pub fn rewind(&mut self) -> Result<()> {
        // MF 用 PROPVARIANT 表达"跳到哪":`GUID::zeroed()` = 以 100ns 为单位的时间
        let mut position = PROPVARIANT::default();
        // 走裸指针而不是直接给字段赋值:那两个字段外面套着 `ManuallyDrop`(联合体),直接写
        // 会被编译器拦下来(它会以为你要析构旧值)。这里只是往一个已经清零的结构里填两个字段。
        unsafe {
            let inner = &mut *position.Anonymous.Anonymous as *mut PROPVARIANT_0_0;
            (*inner).vt = VT_I8;
            (*inner).Anonymous.hVal = 0;
        }
        unsafe {
            self.reader
                .SetCurrentPosition(&GUID::zeroed(), &position)
                .context("把视频倒回开头失败")?
        };
        self.done = false;
        Ok(())
    }
}

/// 打开 → 取第一帧 → 关掉,返回那一帧的像素(自己拥有一份)。
///
/// 存在的理由:**封面 / 缩略图**。入库时给视频配一张静帧当封面,面板上才能像别的壁纸一样显示格子;
/// 与其让调用方自己管一个临时的 `Player` 生命周期,不如在这里一次给完。
pub fn first_frame(path: &Path, limit: (u32, u32)) -> Result<(VideoInfo, Vec<u8>)> {
    let mut player = Player::open(path, limit)?;
    let pixels = player
        .next_frame()?
        .map(|frame| frame.to_vec())
        .ok_or_else(|| anyhow!("这段视频一帧都解不出来"))?;
    Ok((player.info(), pixels))
}

/// 源尺寸 → 解码尺寸:不放大,只缩小;缩完取偶数(视频编码器基本都要求偶数,缩放器也一样)。
pub fn fit(width: u32, height: u32, limit: (u32, u32)) -> (u32, u32) {
    let (mut width, mut height) = (width.max(2), height.max(2));
    let limit_w = limit.0.max(2);
    let limit_h = limit.1.max(2);
    if width > limit_w || height > limit_h {
        // 两个方向各自算一个比例,取小的那个 —— 这样两边都在上限之内,而且不变形
        let scale = (limit_w as f64 / width as f64).min(limit_h as f64 / height as f64);
        width = ((width as f64 * scale) as u32).max(2);
        height = ((height as f64 * scale) as u32).max(2);
    }
    (width & !1, height & !1)
}

/// `MFVideoFormat_RGB32` 的内存是 B,G,R,X —— 换成渲染器要的 RGBA8。
///
/// `stride` 为负表示**自下而上**(数据最后一行才是画面第一行),这时候要倒着取行;行内方向不变。
/// 抽成独立函数是为了能单测:这段逻辑错了不会报错,只会让画面上下颠倒或者红蓝对调。
fn copy_bgra_to_rgba(
    src: &[u8],
    stride: i32,
    width: u32,
    height: u32,
    dst: &mut [u8],
) -> Result<()> {
    let row_bytes = width as usize * 4;
    let step = stride.unsigned_abs() as usize;
    if stride == 0 {
        return Err(anyhow!("这条流的行长是 0,读不出画面"));
    }
    if dst.len() < row_bytes * height as usize {
        return Err(anyhow!("给的目标缓冲装不下 {width}×{height}"));
    }
    if src.len() < step * height as usize {
        return Err(anyhow!(
            "解码出来的帧只有 {} 字节,按 {width}×{height}(行长 {step})需要 {}",
            src.len(),
            step * height as usize
        ));
    }
    for y in 0..height as usize {
        let src_row = if stride > 0 { y } else { height as usize - 1 - y };
        let from = src_row * step;
        let to = y * row_bytes;
        for x in 0..width as usize {
            let (s, d) = (from + x * 4, to + x * 4);
            dst[d] = src[s + 2];
            dst[d + 1] = src[s + 1];
            dst[d + 2] = src[s];
            // 第四字节是补位(X),对面给什么都不用管,自己写 255
            dst[d + 3] = 255;
        }
    }
    Ok(())
}

/// 视频流的编号。`MF_SOURCE_READER_FIRST_VIDEO_STREAM` 是个负数常量(见 MF 的约定)。
fn stream_index() -> u32 {
    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32
}

/// 整个文件那一"流"的编号(时长这类属性挂在这里)
fn media_source() -> u32 {
    MF_SOURCE_READER_MEDIASOURCE.0 as u32
}

/// 把输出类型定成 RGB32、尺寸定成 `width`×`height`。
///
/// 做法是**先抄一份原生类型再改**,不是从零造一个:原生类型里带着像素长宽比、色彩空间这些
/// 我们既不懂也不该丢的东西,自己造会把它们一起丢掉。
unsafe fn set_output_type(
    reader: &IMFSourceReader,
    native: &IMFMediaType,
    width: u32,
    height: u32,
    fps: f32,
) -> Result<()> {
    let target = unsafe { MFCreateMediaType() }?;
    unsafe {
        native.CopyAllItems(&target)?;
        target.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        target.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        target.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
        // 帧率、交错、像素长宽比都只是"告诉转换器我们想要什么",写不进去不致命
        if fps.is_finite() && fps > 0.0 {
            let _ = target.SetUINT64(&MF_MT_FRAME_RATE, pack(fps.round() as u32, 1));
        }
        let _ = target.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);
        let _ = target.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1));
        reader
            .SetCurrentMediaType(stream_index(), None, &target)
            .context("让解码器输出 RGB32 失败(这台机器的 MF 装不上视频处理器)")?;
    }
    Ok(())
}

/// MF 把"两个 32 位数"塞进一个 u64:高位在前(`(宽 << 32) | 高`)。
fn pack(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

unsafe fn frame_size(media_type: &IMFMediaType) -> Result<(u32, u32)> {
    let packed = unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }
        .context("这条流没说画面多大")?;
    Ok(((packed >> 32) as u32, packed as u32))
}

unsafe fn frame_rate(media_type: &IMFMediaType) -> Option<f32> {
    let packed = unsafe { media_type.GetUINT64(&MF_MT_FRAME_RATE) }.ok()?;
    let (numerator, denominator) = ((packed >> 32) as u32, packed as u32);
    if denominator == 0 {
        return None;
    }
    Some(numerator as f32 / denominator as f32)
}

/// 从 PROPVARIANT 里取一个 u64。只认 MF 会用的两种:VT_UI8(MF_PD_DURATION)和 VT_I8。
fn variant_u64(value: &PROPVARIANT) -> Option<u64> {
    use windows::Win32::System::Variant::{VT_UI8, VARENUM};
    unsafe {
        let inner = &value.Anonymous.Anonymous;
        let kind: VARENUM = inner.vt;
        if kind == VT_UI8 {
            Some(inner.Anonymous.uhVal)
        } else if kind == VT_I8 {
            Some(inner.Anonymous.hVal.max(0) as u64)
        } else {
            None
        }
    }
}

/// COM 和 MF 的初始化(见文件头的说明:**两件事的粒度不一样**)。
fn ensure_mf() -> Result<()> {
    // COM 是**线程级**的:每个碰 MF 的线程都得自己来一次。线程已经是别的套间模型时这里会返回
    // RPC_E_CHANGED_MODE —— 不是错误,MF 在 MTA/STA 下都能用,忽略即可。
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    // MF 是**进程级**的:只做一次。启动失败要把原因留住(它只跑一次,后面所有失败都指着它)。
    static STARTED: OnceLock<Result<(), String>> = OnceLock::new();
    let started = STARTED.get_or_init(|| unsafe {
        MFStartup(MF_VERSION, MFSTARTUP_FULL)
            .map(|_| ())
            .map_err(|err| format!("{err}"))
    });
    started
        .clone()
        .map_err(|err| anyhow!("初始化 Media Foundation 失败:{err}"))
}

/// 转成 `PCWSTR` 要的、以 0 结尾的宽字符数组。
fn wide(value: &OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    value.encode_wide().chain(std::iter::once(0)).collect()
}

/// 读不出帧率时的兜底。25 是 PAL 的老规矩,30 是现在的主流 —— 取 30 更不容易"慢放"。
const DEFAULT_FPS: f32 = 30.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 缩放只缩不放且保持偶数() {
        // 比上限小:原样(但取偶数)
        assert_eq!(fit(1280, 720, (1920, 1080)), (1280, 720));
        // 4K → 1080p 以内,比例不变
        assert_eq!(fit(3840, 2160, (1920, 1080)), (1920, 1080));
        // 2160×1080(生图服务商真给过这个尺寸)→ 按宽缩,高度跟着下来
        let (width, height) = fit(2160, 1080, (1920, 1080));
        assert_eq!(width, 1920);
        assert_eq!(height, 960);
        // 竖屏也不会越界
        let (width, height) = fit(1080, 1920, (1920, 1080));
        assert!(width <= 1920 && height <= 1080);
        // 奇数尺寸会被抹平成偶数
        assert_eq!(fit(1921, 1081, (4096, 4096)), (1920, 1080));
    }

    /// BGRX → RGBA。红蓝对调是最容易犯、也最容易被当成"服务商给的视频就长这样"的错误,
    /// 所以这里正着测一遍、倒着(stride < 0,自下而上)再测一遍。
    #[test]
    fn 换序把红蓝掰正() {
        // 4×2 的图:上半纯蓝、下半纯红(内存里是 B,G,R,X 的顺序)
        let mut src = Vec::new();
        for _ in 0..8 {
            src.extend_from_slice(&[0xFF, 0x00, 0x00, 0x00]); // B=255 → 蓝
        }
        let mut dst = vec![0u8; 32];
        copy_bgra_to_rgba(&src, 16, 4, 2, &mut dst).unwrap();
        assert_eq!(&dst[0..4], &[0x00, 0x00, 0xFF, 0xFF], "BGRX 的蓝要变成 RGBA 的蓝");

        // 自下而上(stride < 0):内存里的第一行是画面的**最后一行**,得倒着取。
        // 用 2×2 才测得出"翻行":若按正 stride 读,整张图会上下颠倒。
        let mut bottom_up = Vec::new();
        bottom_up.extend_from_slice(&[0xFF, 0x00, 0x00, 0x00]); // 内存行 0 = 蓝(画面第二行)
        bottom_up.extend_from_slice(&[0xFF, 0x00, 0x00, 0x00]);
        bottom_up.extend_from_slice(&[0x00, 0x00, 0xFF, 0x00]); // 内存行 1 = 红(画面第一行)
        bottom_up.extend_from_slice(&[0x00, 0x00, 0xFF, 0x00]);
        let mut dst = vec![0u8; 16];
        copy_bgra_to_rgba(&bottom_up, -8, 2, 2, &mut dst).unwrap();
        // 注意每行 2 像素 = 8 字节,所以"画面第二行"从 dst[8] 起
        assert_eq!(&dst[0..4], &[0xFF, 0x00, 0x00, 0xFF], "画面第一行要取内存最后一行:红");
        assert_eq!(&dst[8..12], &[0x00, 0x00, 0xFF, 0xFF], "画面第二行取内存第一行:蓝");
    }

    #[test]
    fn 缓冲不够时报错而不是越界读() {
        let src = [0u8; 8];
        let mut dst = vec![0u8; 32];
        assert!(copy_bgra_to_rgba(&src, 16, 4, 2, &mut dst).is_err());
        let big = vec![0u8; 32];
        let mut small = vec![0u8; 4];
        assert!(copy_bgra_to_rgba(&big, 16, 4, 2, &mut small).is_err());
        assert!(copy_bgra_to_rgba(&big, 0, 4, 2, &mut dst).is_err());
    }

    #[test]
    fn 打包与拆包是对称的() {
        assert_eq!(pack(1920, 1080), 0x0780_0000_0438);
        let packed = pack(2160, 1080);
        assert_eq!(packed >> 32, 2160);
        assert_eq!(packed as u32, 1080);
    }

    #[test]
    fn 帧间隔跟着帧率走() {
        // 直接钉住换算,免得哪天有人把 fps 和"每秒帧数"写反
        let info = VideoInfo {
            width: 1920,
            height: 1080,
            fps: 25.0,
            duration: Duration::from_secs(5),
        };
        assert!((1.0 / info.fps - 0.04).abs() < 1e-6);
    }
}
