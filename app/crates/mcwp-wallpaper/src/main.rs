//! MC 壁纸 · 壁纸宿主:把 wgpu 渲染的照片壁纸挂到桌面壁纸层。
//!
//! 用法:
//! ```text
//! mcwp-wallpaper                          # 常驻运行(默认 60fps)
//!                                         # 托盘常驻:打开面板 / 换一张 / 暂停渲染 / 开机自启 / 退出
//! mcwp-wallpaper --seconds 10             # 跑 10 秒自动退出(自检)
//! mcwp-wallpaper --fps 30 --brightness 0.8
//! mcwp-wallpaper --panel                  # 同时打开 Slint 控制面板
//! mcwp-wallpaper --panel --ai-ask "雨夜的城市"    # 开面板,并替用户把这句话发出去
//! mcwp-wallpaper --gen "雨夜的城市"        # 现场生一张图并换上(整条链路,含提示词智能体)
//! mcwp-wallpaper --photo 6a1f2b0c9d3e4f50  # 换上壁纸库里已有的一张(取库目录里的文件名)
//! mcwp-wallpaper --adopt                  # 把**桌面上原来那张壁纸**抬进来当第一张(不生图)
//! mcwp-wallpaper --images-selftest "雪山日出"     # 只跑一遍生图链路,打印结果(不建窗口)
//! mcwp-wallpaper --attach auto            # 挂载策略,见 AttachMode::NAMES
//! mcwp-wallpaper --show-host              # 额外对挂载目标调用 ShowWindow(排查用)
//! mcwp-wallpaper --dump                   # 打印桌面窗口层级(排查用)
//! mcwp-wallpaper --window                 # 不挂桌面,当普通窗口显示(排查用)
//! ```
//!
//! 带「排查用」字样的参数、窗口层级打印、GDI 自检、离屏回读都属于排查脚手架,
//! 只在启用 `dev-tools` 特性时生效:
//! ```text
//! cargo build --release --features dev-tools
//! ```

use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use mcwp_platform::desktop::{
    AttachMode, HostStyle, HostWindow, memory_mb, primary_monitor_rect,
    set_per_monitor_dpi_awareness,
};
#[cfg(feature = "dev-tools")]
use mcwp_platform::desktop::{candidate_report, invalidate_window, set_gdi_background_paint};
#[cfg(feature = "dev-tools")]
use mcwp_platform::diag;
use mcwp_platform::tray::{self, Tray, TrayAction};
use mcwp_render::{PhotoParams, Renderer};
use mcwp_spec::Photo;
use tracing_subscriber::EnvFilter;
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_SUCCESS, GetLastError, SetLastError};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SW_RESTORE, SetForegroundWindow, ShowWindow,
};
use windows::core::{PCWSTR, w};

mod photos;
mod sessions;

use sessions::Sessions;

/// 没配模型时,回复位给的东西。**要能照着做**:直接告诉用户去哪儿填。
const NO_MODEL_REPLY: &str = "还没配模型,这句话先记着。面板左下角「设置」里填上接口地址和一个 \
API Key 就能用了 —— 智谱 GLM-4-Flash 免费,去 bigmodel.cn 注册后把 key 贴进去就行。";

/// 后台线程把生图结果送回主循环的包裹。
///
/// 为什么要绕这一道:生图整条链路是十几秒,主循环是 60 fps —— **绝不能在这里同步等**,
/// 否则整个桌面卡住(还会带着面板一起卡)。所以整轮丢给线程,结果回到主循环再统一落地。
///
/// 失败也走同一条通道,而不是另开一个:两者来自同一轮、要回同一个会话、也要用同一个 `run`
/// 认轮次。分开走只会让"暂停"和"过期丢弃"这两处逻辑各写一遍。
struct AiOutcome {
    /// 这句话属于哪个会话(结果回来时用户可能已经切走了)
    session: u64,
    /// 第几轮。暂停过、或者用户又发了新的一句之后,旧请求的结果会晚一步回来 —— 认轮次丢弃。
    run: u64,
    /// 整条链路的结果:`Ok` 是一张能直接上屏的壁纸,`Err` 是一句给人看的原因。
    result: Result<Box<PhotoPayload>, String>,
}

/// 一轮生图跑完的产物:元数据 + 原始字节 + 解好的位图。
///
/// **解码放在后台线程做**,主循环只负责"贴上去":1080p 的 PNG 解一次上百毫秒,
/// 那点时间放在 60fps 的主循环里就是肉眼可见的卡顿,放在后台线程里完全免费。
struct PhotoPayload {
    reply: String,
    photo: Photo,
    /// 生图接口原样返回的字节,落盘用 —— **不重新编码**,重压一次只会让画面更糊
    bytes: Vec<u8>,
    /// 解好的像素,主线程拿它直接当纹理
    bitmap: mcwp_image::Bitmap,
    /// 提示词那一步是不是走了本地降级
    degraded: bool,
    /// 这张图**实际**带没带服务商那枚「AI生成」角标(兜底重发过就是带,见 `Images::generate`)
    watermarked: bool,
    /// 提示词那一步发了几次请求
    calls: u32,
}

/// 一次「交给模型」的完整任务:跑哪句话 + 属于哪个会话。
///
/// 之所以要留成一份可重复使用的记录:HTTP 请求**没法从半路续上**,所以「继续」只能是"把同一份
/// 任务重跑一遍";而这些字段缺一个都重跑不了 —— 会话 id 决定回复往哪儿放,原话就是请求本身。
#[derive(Debug, Clone)]
struct Mission {
    /// 归属会话(回复回来时用户可能已经切走、甚至把这条删了)
    session: u64,
    /// 用户那句话
    text: String,
}

/// 当前这一轮的处境。面板右下角那颗钮的样子就是它的投影。
#[derive(Debug)]
enum Run {
    /// 已经发出去、结果还没回来的那一轮。`id` 用来认结果:对不上的结果一律丢弃。
    Running { id: u64, mission: Mission },
    /// 被暂停的那一轮:请求已经放弃(结果回来会被丢掉),但用户点「继续」时还要原地重跑。
    Paused(Mission),
}

/// 当前这一轮该显示成什么。
fn run_state(run: Option<&Run>) -> mcwp_ui::RunState {
    match run {
        None => mcwp_ui::RunState::Idle,
        Some(Run::Running { .. }) => mcwp_ui::RunState::Running,
        Some(Run::Paused(_)) => mcwp_ui::RunState::Paused,
    }
}

/// 当前生效的照片壁纸。程序化场景那条路没有它 —— 那正是"两条路"在这里的分水岭。
struct ActivePhoto {
    photo: Photo,
    /// 每帧只改 `time`,其余由照片与屏幕尺寸一次算好
    params: PhotoParams,
    /// **视频壁纸才有**的播放状态。挂在这里而不是另开一份全局状态:
    /// "现在屏幕上是什么"只有一个答案,分两处存就会出现"图换了、视频还在放"这种半拉状态。
    video: Option<VideoPlayback>,
    /// 用户/桌面定下来的那个亮度。视频的曝光校正已经烘进帧像素,不再走这里;
    /// 单存一份基准,面板/托盘改亮度时才不会被上一帧的校正值污染。
    base_brightness: f32,
}

/// 正在放的视频。
///
/// 它是个**拉模式**的播放器(见 `mcwp_video::Player`):主循环每帧问它"该换下一帧了吗",
/// 由它按视频自己的帧率回答。这么设计是因为壁纸的刷新率(60)和视频的帧率(31)对不上 ——
/// 谁快谁慢还不一定,让任一方去驱动另一方都会出错。
struct VideoPlayback {
    player: mcwp_video::Player,
    /// 距离"该换下一帧"还差多久(秒)。每帧减去这一帧的真实耗时
    remaining: f32,
    /// 这一轮**已经放到第几秒**(按真正贴出去的帧累加,重新循环时归零)。
    /// 接缝那一小段全靠它认位置 —— 别拿 `remaining` 反推:那个值为了"落后就丢帧"被夹过。
    elapsed: f32,
    /// 这段多长(秒),来自解码器。
    duration: f32,
    /// **第一帧**的像素。循环点两端都向它收口(见 `LOOP_FADE_SECS`)。
    first: Vec<u8>,
    /// 混合用的缓冲。只在接缝那一小段用到 —— 复用同一个 `Vec`,
    /// 别每帧再 `to_vec()` 一次(1080p 一帧 8 MB,一秒 30 次就是 250 MB/s 的无谓分配)。
    scratch: Vec<u8>,
    /// 整段统一乘的曝光增益(1.0 = 不校)。
    ///
    /// 它来自**原图 vs 视频首帧**的亮度差,不是首末帧的差。旧版把 `seam_gain`
    /// (末帧/首帧)摊到整段,遇到"片头暗、后面亮"的片子会把整段压暗,用户看到的
    /// 就是"动图怎么这么暗"。这里只做一件事:把视频拉回原图的曝光,绝不压暗。
    display_gain: f32,
    /// 片头要跳过的帧数(片头从黑场淡入时不为 0)。
    skip_frames: usize,
}

/// 循环点前后各用多长一段向第一帧收口。
///
/// 为什么非要有这一段:模型给的头尾**不可能一模一样**(哪怕首尾帧都喂了同一张图 ——
/// 它是"尽力绕回去",不是"保证绕回去"),而硬切回去就是那一下肉眼可见的跳。
/// 结尾淡回首帧只解决一半:回到开头后,第一帧和第一个视频帧之间仍可能有一大步
/// (实测森林那条片子平均差 5.1,是整段最大的单帧跳变)。所以开头也用同样的窗口
/// 从首帧慢慢展开到视频画面,循环点才真正两头闭合。
///
/// 为什么是 0.8 秒:短了盖不住,长了能看出"画面在往回并"(运动明显变慢)。
/// 视频短于 3.2 秒时按 1/4 收窄(两个 alpha 函数里各自夹住)—— 整段都在淡等于整段都不对劲。
const LOOP_FADE_SECS: f32 = 0.8;

/// 首尾帧引导默认关闭。
///
/// 2026-10-17 的同图实测:首尾帧光流约 0.06,单图 `quality` 约 2.92、`speed` 约 8.79。
/// 首尾帧会把"最后一帧落到第二张图"当成硬目标,模型于是把中间运动压到几乎静止。
/// 动态壁纸默认走单图首帧;首尾帧只在明确需要"循环锁定/转场"时再打开。
const GUIDED_FRAMES_ENABLED: bool = false;

/// 片头淡入的探测窗口。图生视频常常从黑场淡入(实测森林那条前 0.7 秒从 31 爬到 44),
/// 如果直接拿第 0 帧当循环基准,每次循环都会先暗半秒。这里在开头扫一小段:
/// 亮度明显爬升就跳过这段淡入,拿稳定后的那帧当首帧。
const VIDEO_START_SCAN_SECS: f32 = 0.8;

impl VideoPlayback {
    /// 打开一段视频并把第一帧贴上去。
    fn open(
        renderer: &mut Renderer,
        path: &std::path::Path,
        screen: (u32, u32),
        target_mean: Option<f32>,
    ) -> Result<Self> {
        let mut player = mcwp_video::Player::open(path, screen)
            .with_context(|| format!("打开视频失败:{}", path.display()))?;
        let info = player.info();
        let step = player.frame_interval().max(0.001);
        let (frame, skip_frames) = first_display_frame(&mut player, info.duration.as_secs_f32())?;
        let first_mean = rgb_mean(&frame);
        let display_gain = display_gain_for(target_mean, first_mean);
        let first = scale_rgb(&frame, display_gain);
        renderer.set_video_frame(info.width, info.height, &first)?;
        let duration = (info.duration.as_secs_f32() - skip_frames as f32 * step).max(step);
        Ok(Self {
            player,
            remaining: 0.0,
            elapsed: 0.0,
            duration,
            first,
            scratch: Vec::new(),
            display_gain,
            skip_frames,
        })
    }

    /// 走一帧的时钟;该换帧了就把新的一帧贴上去。
    ///
    /// `delta` 是**这一帧实际过了多久**(不是"应该在多久之后")——暂停几分钟再继续时,
    /// 这里会一次性收到一个很大的 delta,所以下面要把欠下的帧数夹住(见"落后就丢帧")。
    fn tick(&mut self, renderer: &mut Renderer, delta: f32) -> Result<()> {
        self.remaining += delta;
        let step = self.player.frame_interval();
        if self.remaining < step {
            return Ok(());
        }
        // **落后就丢帧,不追**:卡一下没人看得出来,但一口气解十帧会把这一帧拉长十倍,
        // 于是"卡一下"变成"卡一秒"。夹住上限之后,视频最多慢一点点,永远追得回来。
        self.remaining = (self.remaining - step).min(step);

        let info = self.player.info();
        // 强度按**这一帧的时间**算,然后时间往后走一帧:刚开头那一帧的时间是 0(强度也是 0),
        // 一循环完立刻归零,不会在接缝上多算一帧。
        // 结尾向首帧收、开头从首帧展开。两个窗口不会同时出现(短片会按 1/4 收窄),
        // 取较大的那个即可:在两端都是 alpha=1,离开端点后平滑衰减到 0。
        let alpha = loop_fade_alpha(self.elapsed, self.duration)
            .max(loop_intro_alpha(self.elapsed, self.duration));
        let gain = self.display_gain;
        let skip_frames = self.skip_frames;
        self.elapsed = (self.elapsed + step).min(self.duration);
        // 借用要拆开:`next_frame` 会把 `player` 借走,而贴帧还要用 `first` / `scratch`。
        let mut restarted = false;
        let Self {
            player,
            first,
            scratch,
            ..
        } = self;
        match player.next_frame()? {
            Some(pixels) => present_frame(
                renderer,
                info.width,
                info.height,
                pixels,
                alpha,
                gain,
                first,
                scratch,
            )?,
            // 放完了:**从头再来**。生图服务商给的只有十秒左右,循环播放才像一张壁纸;
            // 停在最后一帧会让桌面看起来"死"了。最后那几帧已经淡回第一帧(见 `LOOP_FADE_SECS`),
            // 所以这一次重头开始是**接得上**的 —— 原来这里是一记硬切,用户看到的就是那一下跳。
            None => {
                player.rewind()?;
                restarted = true;
                // 回到片头后同样跳过淡入段。`first` 已经是**校正过曝光**的基准帧,
                // 所以这里直接贴它,不再乘一次增益 —— 接缝两边才是同一个像素值。
                for _ in 0..skip_frames.max(1) {
                    if player.next_frame()?.is_none() {
                        break;
                    }
                }
                present_frame(
                    renderer,
                    info.width,
                    info.height,
                    first,
                    0.0,
                    1.0,
                    first,
                    scratch,
                )?;
            }
        }
        if restarted {
            self.elapsed = 0.0;
        }
        Ok(())
    }
}

/// 找出第一帧该拿哪一帧:片头明显从黑场淡入时,跳过淡入段。
///
/// 返回 `(帧像素, 跳过的帧数)`。跳过的帧数会存进 `VideoPlayback`,每次循环回到
/// 片头都按同样的数量再跳一次 —— 否则第一轮是亮的,第二轮又从黑场开始。
fn first_display_frame(player: &mut mcwp_video::Player, duration: f32) -> Result<(Vec<u8>, usize)> {
    let step = player.frame_interval().max(0.001);
    let scan_secs = VIDEO_START_SCAN_SECS.min(duration.max(0.0) * 0.25);
    let scan = ((scan_secs / step).round() as usize).clamp(1, 60);
    let total = (duration / step).floor() as usize;
    let mut first = None;
    let mut last = None;
    let mut consumed = 0usize;
    for _ in 0..scan {
        let Some(pixels) = player.next_frame()? else {
            break;
        };
        let frame = pixels.to_vec();
        if first.is_none() {
            first = Some(frame.clone());
        }
        last = Some(frame);
        consumed += 1;
    }
    let first = first.ok_or_else(|| anyhow::anyhow!("视频里一帧画面都没有"))?;
    let last = last.unwrap_or_else(|| first.clone());
    let (first_mean, last_mean) = (rgb_mean(&first), rgb_mean(&last));
    if consumed > 1
        && total > consumed + 1
        && first_mean.is_finite()
        && last_mean.is_finite()
        && last_mean > first_mean * 1.08
    {
        return Ok((last, consumed));
    }
    player.rewind()?;
    let frame = player
        .next_frame()?
        .map(|pixels| pixels.to_vec())
        .ok_or_else(|| anyhow::anyhow!("视频里一帧画面都没有"))?;
    Ok((frame, 0))
}

/// 原图亮度 → 视频首帧亮度,算一个**只提不压**的曝光增益。
///
/// 旧版拿首末帧的比值摊到整段,片头暗、后面亮的片子会被压暗 25% —— 这正是
/// 用户看到的"动图这么暗"。这里只把视频拉回原图的曝光;原图更暗时保持原样,
/// 因为把已经够亮的画面压暗没有任何收益。
fn display_gain_for(target_mean: Option<f32>, first_mean: f32) -> f32 {
    let Some(target) = target_mean else {
        return 1.0;
    };
    if !target.is_finite() || !first_mean.is_finite() || target <= 1.0 || first_mean <= 1.0 {
        return 1.0;
    }
    let gain = (target / first_mean).clamp(1.0, 1.35);
    if (gain - 1.0).abs() < 0.02 { 1.0 } else { gain }
}

/// 对整帧的 RGB 乘一个增益(alpha 通道不动)。
fn scale_rgb(pixels: &[u8], gain: f32) -> Vec<u8> {
    if !gain.is_finite() || (gain - 1.0).abs() <= 0.002 {
        return pixels.to_vec();
    }
    let gain = gain.clamp(0.5, 2.0);
    let mut out = pixels.to_vec();
    for pixel in out.chunks_exact_mut(4) {
        for channel in 0..3 {
            pixel[channel] = (pixel[channel] as f32 * gain).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// 循环点的混合强度:0 = 照原样,1 = 完全是第一帧。
///
/// 写成纯函数是为了能单测 —— 这段的取值区间(什么时候开始淡、淡到多少)**肉眼看不出来**,
/// 只能靠测试钉住两端的边界。
fn loop_fade_alpha(elapsed: f32, duration: f32) -> f32 {
    // 短片别整段都在淡:那等于整段都不对劲
    let fade = LOOP_FADE_SECS.min(duration * 0.25);
    if fade <= 0.0 || duration <= fade {
        return 0.0;
    }
    let u = (elapsed - (duration - fade)) / fade;
    if u <= 0.0 {
        return 0.0;
    }
    // smoothstep:两端斜率为 0,"开始淡了"那一刻看不出来
    let u = u.min(1.0);
    u * u * (3.0 - 2.0 * u)
}

/// 开头的收口强度:1 = 完全是第一帧,0 = 已经是视频自己的画面。
///
/// 和 `loop_fade_alpha` 对称,只是方向反过来。没有它,循环回到开头时第一帧到第二帧
/// 仍然会硬跳一下 —— 结尾淡回首帧只把"末帧→首帧"接上了,没接"首帧→视频"。
fn loop_intro_alpha(elapsed: f32, duration: f32) -> f32 {
    let fade = LOOP_FADE_SECS.min(duration * 0.25);
    if fade <= 0.0 || duration <= fade {
        return 0.0;
    }
    let u = 1.0 - elapsed / fade;
    if u <= 0.0 {
        return 0.0;
    }
    let u = u.min(1.0);
    u * u * (3.0 - 2.0 * u)
}

/// 贴一帧:先把曝光校正烘进这一帧,再和第一帧做接缝混合,最后上屏。
///
/// 曝光校正**必须在混合之前做**。之前走的是渲染器的全局亮度,结果是:最后一帧已经
/// 淡成了第一帧(alpha = 1),却又被乘了一次 0.75 的曝光增益,循环点就从"首帧"硬跳到
/// "首帧 × 0.75"。把增益放进像素处理里,alpha = 1 时结果严格等于首帧,接缝才真的闭合。
fn present_frame(
    renderer: &mut Renderer,
    width: u32,
    height: u32,
    pixels: &[u8],
    alpha: f32,
    gain: f32,
    first: &[u8],
    scratch: &mut Vec<u8>,
) -> Result<()> {
    if compose_video_frame(pixels, first, alpha, gain, scratch) {
        renderer.set_video_frame(width, height, scratch)?;
    } else {
        renderer.set_video_frame(width, height, pixels)?;
    }
    Ok(())
}

/// 曝光校正 + 接缝混合。返回 `false` 表示这一帧不用改,直接贴原像素。
///
/// 单独拆出来是为了能单测最关键的一条:**alpha = 1 时,输出必须严格等于第一帧**,
/// 不管曝光增益是多少。循环点之所以会跳,就是这条曾经不成立。
fn compose_video_frame(
    pixels: &[u8],
    first: &[u8],
    alpha: f32,
    gain: f32,
    scratch: &mut Vec<u8>,
) -> bool {
    let gain = if gain.is_finite() {
        gain.clamp(0.5, 2.0)
    } else {
        1.0
    };
    let alpha = if alpha.is_finite() {
        alpha.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let blend = alpha > 0.0 && pixels.len() == first.len();
    let correct = (gain - 1.0).abs() > 0.002;
    // 不用混、也不用校 → 直接贴原帧,别为看不出的变化多拷一份 1080p 像素。
    if !blend && !correct {
        return false;
    }
    scratch.clear();
    scratch.extend_from_slice(pixels);
    // 只动 RGB —— alpha 通道这一路恒为 255,混它没意义。
    // 先乘增益,再按 alpha 向首帧靠;alpha = 1 时无论增益是多少,结果都是首帧原像素。
    for (index, dst) in scratch.chunks_exact_mut(4).enumerate() {
        let target = if blend {
            Some(&first[index * 4..index * 4 + 4])
        } else {
            None
        };
        for channel in 0..3 {
            let mut value = dst[channel] as f32 * gain;
            if let Some(target) = target {
                value += (target[channel] as f32 - value) * alpha;
            }
            dst[channel] = value.round().clamp(0.0, 255.0) as u8;
        }
    }
    true
}

/// 一轮「做成动态壁纸」跑完的产物。
///
/// 和 `PhotoPayload` 是**两件事**,不是一个东西加个字段:生图那轮的产物是"一张图的字节",
/// 这轮的产物是"一段视频 + 它的封面"。混成一个结构就得给一半字段写"另一支不用"的注释,
/// 而两边的失败处理也不一样(生图失败 = 没画出来,这条失败 = 静图还是好的,只是没变成视频)。
struct VideoPayload {
    /// 元数据(`kind` 已经是 `Video`)
    photo: Photo,
    /// 图生视频接口给的 mp4 原始字节
    mp4: Vec<u8>,
    /// 封面(视频第一帧)。入库时它是那条记录的"画面文件",也是缩略图的来源
    cover: mcwp_image::Bitmap,
}

/// 后台线程把「做成动态壁纸」的结果送回主循环。理由和 `AiOutcome` 一样:
/// 这条路要跑一分钟,不能占着 60fps 的主循环;而且它**比生图还长**,更等不起。
struct VideoOutcome {
    /// 第几轮。同一时刻只允许跑一轮(面板那颗钮会拦住),留它是为了万一
    /// 以后放开并发时,旧结果能自己认出来被丢掉。
    run: u64,
    result: Result<Box<VideoPayload>, VideoFailure>,
}

/// 一轮「做成动态壁纸」失败时带回来的现场。
///
/// `task_id` 是关键:提交成功之后才失败的那条任务,服务商那边**已经计过费**,
/// 任务号能让用户点「免费取回」把成片捡回来。`source_id` 是那张静图的 id,
/// 主循环靠它把任务号写回会话里对应的消息。
struct VideoFailure {
    message: String,
    task_id: Option<String>,
    source_id: String,
}

/// 当前 unix 秒。壁纸库靠它排序,也参与文件名。
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// 新消息:先记进会话,再跑。
fn start_run(
    text: &str,
    sessions: &mut Sessions,
    chat: &Option<mcwp_ai::Client>,
    images: &Option<mcwp_ai::Images>,
    current: Option<Photo>,
    screen: (u32, u32),
    tx: &Sender<AiOutcome>,
    id: u64,
) -> (&'static str, Option<Mission>) {
    let mission = Mission {
        session: sessions.send(text),
        text: text.to_string(),
    };
    run_mission(mission, sessions, chat, images, current, screen, tx, id)
}

/// 把一份任务丢给后台线程(新消息和「继续」走的都是这一条路)。
///
/// 没配模型时当场把提示写进会话、返回 `None` —— 那种情况没有"在跑"这一轮,面板的钮也就不该
/// 变成暂停键。**绝不在这里同步等**(见 `AiOutcome` 的说明)。
fn run_mission(
    mission: Mission,
    sessions: &mut Sessions,
    chat: &Option<mcwp_ai::Client>,
    images: &Option<mcwp_ai::Images>,
    current: Option<Photo>,
    screen: (u32, u32),
    tx: &Sender<AiOutcome>,
    id: u64,
) -> (&'static str, Option<Mission>) {
    let (Some(chat), Some(images)) = (chat.as_ref(), images.as_ref()) else {
        // 没配也得有回音:不然用户发完消息看着空荡荡的右栏,只会以为程序坏了
        sessions.reply(mission.session, NO_MODEL_REPLY);
        return ("还没配生图 · 去左下角「设置」里填一个 API Key", None);
    };
    let chat = chat.clone();
    let images = images.clone();
    let request = mission.text.clone();
    let session = mission.session;
    let tx = tx.clone();
    std::thread::spawn(move || {
        let result = generate_photo(&chat, &images, &request, current.as_ref(), screen)
            .map(Box::new)
            .map_err(|err| format!("{err:#}"));
        // 主线程收摊了(托盘退出)就随它去,没人接的结果没有意义
        let _ = tx.send(AiOutcome {
            session,
            run: id,
            result,
        });
    });
    ("正在画一张新壁纸…", Some(mission))
}

/// 生图整条链路(**在后台线程里跑**)。三步顺序不能换:
///
/// 1. `plan_photo` —— 文本模型把用户那句话改写成生图提示词 + 动效配方;
/// 2. `Images::generate` —— 出图并当场把图下下来(整轮的等待几乎全在这一步,9~16 s);
/// 3. 解码 + 提调色板 —— 拿到像素和 5 个颜色,主循环就不用再碰这些。
///
/// 第 1 步失败**不致命**(`plan_photo` 会降级成"拿原话去画"),第 2 步失败才是真的没图。
fn generate_photo(
    chat: &mcwp_ai::Client,
    images: &mcwp_ai::Images,
    request: &str,
    current: Option<&Photo>,
    screen: (u32, u32),
) -> Result<PhotoPayload> {
    let plan = chat.plan_photo(request, current);
    let generated = images.generate(&plan.image_prompt, screen.0, screen.1)?;
    let bitmap = mcwp_image::Bitmap::decode(&generated.bytes)
        .context("生图返回的字节解不开(不是 PNG / JPEG?)")?;
    let created = now_secs();
    let mut photo = Photo {
        title: plan.title,
        prompt: plan.image_prompt,
        // 记**实际出图尺寸**而不是请求尺寸:服务商可能自己改,以后要按它算 cover
        width: bitmap.width,
        height: bitmap.height,
        motion: plan.motion,
        palette: mcwp_spec::palette_from_rgba(&bitmap.rgba, bitmap.width, bitmap.height),
        // 效果层的随机盐取生成时间:同一张壁纸每次重放花纹都一样(seed 存在 JSON 里),
        // 而同一条提示词再生成一次是另一张图 —— 花纹跟着换,正好。
        seed: (created & 0xffff_ffff) as u32,
        created,
        ..Photo::default()
    };
    photo.normalize();
    Ok(PhotoPayload {
        reply: plan.reply,
        photo,
        bytes: generated.bytes,
        bitmap,
        degraded: plan.degraded,
        watermarked: generated.watermarked,
        calls: plan.calls,
    })
}

/// 把一张照片装到屏幕上:换纹理 + 重算着色器参数。
///
/// 抽出来是因为有四个入口都要它(`--gen` / `--photo` / 点库里的卡片 / 托盘"换一张")。
/// 少一处没同步,症状都是"点了没反应但日志说切了" —— 那种 bug 找起来最费时间。
fn show_photo(
    renderer: &mut Renderer,
    photo: Photo,
    bitmap: &mcwp_image::Bitmap,
    screen: (u32, u32),
    brightness: f32,
) -> Result<ActivePhoto> {
    renderer.set_photo(bitmap.width, bitmap.height, &bitmap.rgba)?;
    Ok(ActivePhoto {
        params: PhotoParams::from_photo(&photo, screen.0, screen.1, brightness),
        photo,
        // 静图:上屏时把播放器丢掉 —— 它可能正是上一张动态壁纸留下来的
        video: None,
        base_brightness: brightness,
    })
}

/// 换上一段视频(库里那条 `<id>.mp4`)。
///
/// 曝光基准尽量用**原图**:图生视频的成片常常比原图暗一截,播放时按原图的平均亮度
/// 把整段提亮回来。原图找不到(手工导入的视频)就保持 1.0,不乱猜。
fn source_mean_for(photo: &Photo) -> Option<f32> {
    let origin = photo.origin_id.trim();
    if origin.is_empty() {
        return None;
    }
    let entry = photos::load(origin).ok()?;
    let bitmap = entry.read_image().ok()?;
    let mean = rgb_mean(&bitmap.rgba);
    (mean.is_finite() && mean > 1.0).then_some(mean)
}

fn show_video(
    renderer: &mut Renderer,
    photo: Photo,
    path: &std::path::Path,
    screen: (u32, u32),
    brightness: f32,
) -> Result<ActivePhoto> {
    let video = VideoPlayback::open(renderer, path, screen, source_mean_for(&photo))?;
    Ok(ActivePhoto {
        // 参数用 `from_video`:关掉 Ken Burns 和效果层(见那里的说明)
        params: PhotoParams::from_video(&photo, screen.0, screen.1, brightness),
        photo,
        video: Some(video),
        base_brightness: brightness,
    })
}

/// 起一轮「做成动态壁纸」,返回一行给面板看的话。
///
/// 两个入口(库格子的右键菜单、库页工具栏那颗钮)都走这里:能不能跑、跑过了没、库里第几张,
/// 这些判断只写一遍 —— 分散在两处的话,迟早出现"一个入口拦住了、另一个没拦"。
fn start_video_run(
    chat: &Option<mcwp_ai::Client>,
    videos: &Option<mcwp_ai::Videos>,
    library: &[LibraryEntry],
    index: usize,
    screen: (u32, u32),
    tx: &Sender<VideoOutcome>,
    run: &mut Option<u64>,
    run_id: u64,
) -> String {
    let Some(animator) = videos.as_ref() else {
        return "还没配图生视频的 key —— 它和生图是同一把,在「设置」里填上就能用".to_string();
    };
    // 同时只跑一条:它要一分钟、还要花钱,点两下就并排跑两条是纯浪费。
    if run.is_some() {
        return "正在做上一段动态壁纸,等它出来再说".to_string();
    }
    let Some(entry) = library.get(index) else {
        return format!("壁纸库里没有第 {index} 张");
    };
    if entry.source.is_video() {
        return "这一张已经是动态壁纸了".to_string();
    }
    let title = entry.source.photo.title.clone();
    let source = entry.source.clone();
    let source_id = source.photo.id.clone();
    let chat = chat.clone();
    let animator = animator.clone();
    *run = Some(run_id);
    // 和生图一样丢给后台线程:这条要跑一分钟,占着主循环整个桌面都会卡。
    // 发送端要 clone 一份搬进线程(`Sender` 的 clone 很便宜,共享同一个队列)。
    let sender = tx.clone();
    std::thread::spawn(move || {
        let result = match animate_photo(chat.as_ref(), &animator, &source, screen) {
            Ok(payload) => Ok(Box::new(payload)),
            Err(err) => {
                // 把任务号从结构化错误里拿出来;没有就说明还没提交成功,
                // 那种失败不该给「免费取回」(也没有东西可取)。
                let task_id = err
                    .downcast_ref::<mcwp_ai::VideoTaskFailure>()
                    .map(|failure| failure.task_id.clone());
                Err(VideoFailure {
                    message: format!("{err:#}"),
                    task_id,
                    source_id,
                })
            }
        };
        let _ = sender.send(VideoOutcome {
            run: run_id,
            result,
        });
    });
    // 别说"约一分钟":实测能从一分钟拉到十几分钟(排队),说短了用户会以为它卡死了
    format!("正在把「{title}」做成动态壁纸(要几分钟,高峰会更久;期间可以接着聊)")
}

/// `start_video_run` 的第二半:真起跑了就**把面板上那一条切成"正在生成动图…"**。
///
/// 为什么非要有这一层:三个入口(库格子的右键菜单、库页工具栏那颗钮、会话里那条回复下面的
/// 「生成动图」)都要做"起跑 + 点亮"这两件事,而且**两半的判据必须一致** —— 被拦下来时
/// (库空 / 已经是视频 / 已经有一条在跑)面板上那颗钮要原样熄灭。散在三处抄三遍的话,
/// 迟早有一个入口漏掉点亮(用户点完毫无反应,这次改动修的正是那个)或者漏掉熄灭
/// (那条回复永远停在"正在生成")。
///
/// 判据就是 `run`:**只有 `start_video_run` 真的把它写上去了才是起跑了**。
fn start_video_run_visible(
    chat: &Option<mcwp_ai::Client>,
    videos: &Option<mcwp_ai::Videos>,
    library: &[LibraryEntry],
    index: usize,
    screen: (u32, u32),
    tx: &Sender<VideoOutcome>,
    run: &mut Option<u64>,
    run_id: u64,
    target: &mut Option<String>,
    panel: Option<&mcwp_ui::PanelHandle>,
) -> String {
    let status = start_video_run(chat, videos, library, index, screen, tx, run, run_id);
    if *run == Some(run_id) {
        if let Some(id) = library
            .get(index)
            .map(|entry| entry.source.photo.id.clone())
        {
            *target = Some(id.clone());
            if let Some(panel) = panel {
                panel.set_animating(id);
            }
        }
    }
    status
}

#[derive(Debug, Clone, Copy)]
struct TailWarp {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
    dx: f32,
    dy: f32,
    wave: bool,
    phase: f32,
    cloud_like_only: bool,
}

/// 尾帧的局部引导位移:只在会动的元素区域里挪动几个像素,**画面构图、相机位置和静态物体都不动**。
///
/// 首尾帧服务商需要一个明确的终点。尾帧如果和首帧一模一样,模型会选择"保持静止";如果尾帧
/// 让画面整体缩放,那就是推镜。这里的做法是把首帧复制一份,只对天空、草地、树叶等区域做
/// 很小的局部形变 —— 模型看到了元素在动,同时被两端一致的构图钉住镜头。
fn motion_guide_bitmap(
    bitmap: &mcwp_image::Bitmap,
    plan: &mcwp_spec::MotionPlan,
) -> (mcwp_image::Bitmap, usize, f32) {
    let warps = motion_guide_warps(bitmap.width, bitmap.height, plan);
    if warps.is_empty() || bitmap.is_empty() {
        return (bitmap.clone(), 0, 0.0);
    }

    let width = bitmap.width as usize;
    let height = bitmap.height as usize;
    let mut rgba = vec![0u8; bitmap.rgba.len()];
    let mut max_shift = 0.0f32;
    for y in 0..height {
        let ny = (y as f32 + 0.5) / height as f32;
        for x in 0..width {
            let nx = (x as f32 + 0.5) / width as f32;
            let offset = (y * width + x) * 4;
            let pixel = &bitmap.rgba[offset..offset + 4];
            let mut sx = x as f32;
            let mut sy = y as f32;
            for warp in &warps {
                let mut mask = warp_mask(nx, ny, warp);
                if mask <= 0.0 {
                    continue;
                }
                if warp.cloud_like_only {
                    mask *= cloud_like_weight(pixel);
                }
                if mask <= 0.0 {
                    continue;
                }
                let phase = std::f32::consts::TAU * (ny * 2.6 + nx * 0.6) + warp.phase;
                let wave = if warp.wave { phase.sin() } else { 1.0 };
                let cross = if warp.wave { (phase * 0.8).cos() } else { 1.0 };
                sx += warp.dx * mask * wave;
                sy += warp.dy * mask * cross;
                max_shift = max_shift.max((warp.dx * mask).abs().max((warp.dy * mask).abs()));
            }
            let sampled = sample_bilinear(bitmap, sx, sy);
            rgba[offset..offset + 4].copy_from_slice(&sampled);
        }
    }
    (
        mcwp_image::Bitmap {
            width: bitmap.width,
            height: bitmap.height,
            rgba,
        },
        warps.len(),
        max_shift,
    )
}

fn motion_guide_warps(width: u32, height: u32, plan: &mcwp_spec::MotionPlan) -> Vec<TailWarp> {
    let mut warps = Vec::new();
    let mut seen: Vec<&'static str> = Vec::new();
    for (index, element) in plan.elements.iter().enumerate() {
        let kind = element.kind.to_ascii_lowercase();
        let class = tail_kind_class(&kind);
        if class == "light" || seen.contains(&class) {
            continue;
        }
        seen.push(class);
        let words = format!("{} {}", element.action, element.sentence).to_ascii_lowercase();
        let direction = if words.contains("left") {
            -1.0
        } else if words.contains("right") {
            1.0
        } else if index % 2 == 0 {
            -1.0
        } else {
            1.0
        };
        let phase = 0.7 + index as f32 * 1.9;
        // 位移按 1080p 给足:**旧值是 7~13 px,视频模型基本当成静帧**(实测光流
        // 0.06~0.12,肉眼看不出元素在动)。现在草地/蕨类约 30 px、树叶约 19 px、
        // 天空约 38 px —— 只有会动的元素在动,树干、路面和构图仍然像素级不动。
        let (dx, dy, wave, cloud_like_only) = match class {
            "sky" => (width as f32 * 0.020, 0.0, false, true),
            "water" => (width as f32 * 0.014, height as f32 * 0.0030, true, false),
            "ground" => (width as f32 * 0.016, height as f32 * 0.0035, true, false),
            "foliage" => (width as f32 * 0.010, height as f32 * 0.0020, true, false),
            "traffic" => (width as f32 * 0.020, 0.0, false, false),
            "creature" => (
                width as f32 * 0.010,
                -(height as f32) * 0.0025,
                false,
                false,
            ),
            "soft" => (width as f32 * 0.012, 0.0, true, false),
            _ => (width as f32 * 0.010, height as f32 * 0.0015, true, false),
        };
        if class == "ground" {
            for (left, right, side) in [(0.0, 0.45, -1.0), (0.55, 1.0, 1.0)] {
                warps.push(TailWarp {
                    left,
                    top: 0.48,
                    right,
                    bottom: 1.0,
                    dx: dx * side,
                    dy,
                    wave,
                    phase: phase + left,
                    cloud_like_only,
                });
            }
        } else {
            let (left, top, right, bottom) = tail_area_bounds(&element.area, class);
            warps.push(TailWarp {
                left,
                top,
                right,
                bottom,
                dx: dx * direction,
                dy,
                wave,
                phase,
                cloud_like_only,
            });
        }
    }
    warps.truncate(6);
    warps
}

fn tail_kind_class(kind: &str) -> &'static str {
    if [
        "sun",
        "sunlight",
        "sunset",
        "sunrise",
        "glow",
        "light",
        "shadow",
        "exposure",
        "brightness",
        "color",
        "colour",
        "flare",
    ]
    .iter()
    .any(|word| kind.contains(word))
    {
        return "light";
    }
    if ["cloud", "fog", "mist", "smoke", "haze", "sky"]
        .iter()
        .any(|word| kind.contains(word))
    {
        return "sky";
    }
    if ["water", "river", "lake", "sea", "ocean", "wave", "ripple"]
        .iter()
        .any(|word| kind.contains(word))
    {
        return "water";
    }
    if ["grass", "fern", "flower", "bush", "plant", "moss"]
        .iter()
        .any(|word| kind.contains(word))
    {
        return "ground";
    }
    if ["leaf", "leaves", "tree", "branch", "canopy", "forest"]
        .iter()
        .any(|word| kind.contains(word))
    {
        return "foliage";
    }
    if ["car", "traffic", "vehicle", "truck", "bus", "train"]
        .iter()
        .any(|word| kind.contains(word))
    {
        return "traffic";
    }
    if ["bird", "insect", "butterfly"]
        .iter()
        .any(|word| kind.contains(word))
    {
        return "creature";
    }
    if ["fabric", "cloth", "curtain", "hair", "clothing", "clothes"]
        .iter()
        .any(|word| kind.contains(word))
    {
        return "soft";
    }
    "other"
}

fn tail_area_bounds(area: &str, class: &str) -> (f32, f32, f32, f32) {
    let area = area.trim().to_ascii_lowercase();
    match area.as_str() {
        "左上" => (0.0, 0.0, 0.34, 0.34),
        "上中" => (0.33, 0.0, 0.67, 0.34),
        "右上" => (0.66, 0.0, 1.0, 0.34),
        "左中" => (0.0, 0.33, 0.34, 0.67),
        "正中" => (0.33, 0.33, 0.67, 0.67),
        "右中" => (0.66, 0.33, 1.0, 0.67),
        "左下" => (0.0, 0.66, 0.34, 1.0),
        "下中" => (0.33, 0.66, 0.67, 1.0),
        "右下" => (0.66, 0.66, 1.0, 1.0),
        "全图" => (0.0, 0.0, 1.0, 1.0),
        _ if area.contains("sky") || area.contains("upper") || area.contains("background") => {
            (0.0, 0.0, 1.0, 0.46)
        }
        _ if area.contains("foreground") || area.contains("front") || area.contains("lower") => {
            (0.0, 0.46, 1.0, 1.0)
        }
        _ if area.contains("midground") || area.contains("middle") || area.contains("center") => {
            (0.0, 0.14, 1.0, 0.84)
        }
        _ if area.contains("left") => (0.0, 0.14, 0.5, 0.9),
        _ if area.contains("right") => (0.5, 0.14, 1.0, 0.9),
        _ if class == "sky" => (0.0, 0.0, 1.0, 0.46),
        _ if class == "ground" || class == "water" || class == "traffic" => (0.0, 0.46, 1.0, 1.0),
        _ => (0.0, 0.14, 1.0, 0.84),
    }
}

fn warp_mask(nx: f32, ny: f32, warp: &TailWarp) -> f32 {
    let feather_x = ((warp.right - warp.left) * 0.30).max(0.02);
    let feather_y = ((warp.bottom - warp.top) * 0.30).max(0.02);
    smooth01((nx - warp.left) / feather_x)
        * smooth01((warp.right - nx) / feather_x)
        * smooth01((ny - warp.top) / feather_y)
        * smooth01((warp.bottom - ny) / feather_y)
}

fn smooth01(value: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    value * value * (3.0 - 2.0 * value)
}

fn cloud_like_weight(pixel: &[u8]) -> f32 {
    let r = pixel[0] as f32 / 255.0;
    let g = pixel[1] as f32 / 255.0;
    let b = pixel[2] as f32 / 255.0;
    let luma = r * 0.2126 + g * 0.7152 + b * 0.0722;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let saturation = if max <= 1e-5 { 0.0 } else { (max - min) / max };
    smooth01((luma - 0.34) / 0.34) * (1.0 - smooth01((saturation - 0.18) / 0.28))
}

fn sample_bilinear(bitmap: &mcwp_image::Bitmap, x: f32, y: f32) -> [u8; 4] {
    let width = bitmap.width as i32;
    let height = bitmap.height as i32;
    let x = x.clamp(0.0, (width - 1) as f32);
    let y = y.clamp(0.0, (height - 1) as f32);
    let x0 = x.floor() as i32;
    let y0 = y.floor() as i32;
    let x1 = (x0 + 1).min(width - 1);
    let y1 = (y0 + 1).min(height - 1);
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;
    let at = |px: i32, py: i32| {
        let offset = ((py * width + px) * 4) as usize;
        &bitmap.rgba[offset..offset + 4]
    };
    let p00 = at(x0, y0);
    let p10 = at(x1, y0);
    let p01 = at(x0, y1);
    let p11 = at(x1, y1);
    let mut out = [0u8; 4];
    for channel in 0..4 {
        let top = p00[channel] as f32 * (1.0 - fx) + p10[channel] as f32 * fx;
        let bottom = p01[channel] as f32 * (1.0 - fx) + p11[channel] as f32 * fx;
        out[channel] = (top * (1.0 - fy) + bottom * fy).round().clamp(0.0, 255.0) as u8;
    }
    out
}

/// 把库里某一张(静图)做成动态壁纸。**在后台线程里跑**,产物从 `VideoOutcome` 回来。
///
/// 四步:
/// 1. **看图**(免费视觉模型)写一份**元素级**方案:灯在哪儿怎么闪、车流朝哪边走(`plan_motion_vision`)。
///    这一步是"画面真的活起来"和"整张图被推近"之间的分水岭 —— 看不到画面的模型只会写万能运镜。
/// 2. 把方案拼成**图生视频那句提示词**(`video_prompt`);方案是空的就退回老路(文本模型 / 通用运镜)。
/// 3. **图生视频**:默认只发原图当首帧,让模型自己找实体运动。首尾帧引导只在
///    `GUIDED_FRAMES_ENABLED` 打开时启用(见 `motion_guide_bitmap` /
///    `mcwp_ai::animate_frames`)—— 实测它会把运动压成静帧,不适合做默认路径。
/// 4. 把 mp4 落成临时文件、解出第一帧当封面。
///
/// 第 4 步为什么要绕一下:解码器是从**文件**读的(Media Foundation 没有"从内存解一段视频"
/// 的现成路径),而封面必须在主线程入库之前就拿到(入库要连着封面一起写)。所以这里落一份
/// 临时文件,解完**立刻删掉** —— 正式的 mp4 由主线程写进壁纸库。
fn animate_photo(
    chat: Option<&mcwp_ai::Client>,
    videos: &mcwp_ai::Videos,
    entry: &photos::Entry,
    screen: (u32, u32),
) -> Result<VideoPayload> {
    let source = entry.photo.clone();
    // 条件图用**库里那张原图的字节**,不重新编码(见 `mcwp_ai::image_data_url`)
    let image_bytes = std::fs::read(&entry.image)
        .with_context(|| format!("读不出这张图:{}", entry.image.display()))?;
    // 1 · 元素级方案。库里已经存过一份就**直接用** —— 同一张图重做一遍时不必再看第二眼,
    //     两次分析给的元素也未必一样(见 `MotionPlan` 的说明)。
    let plan = match chat {
        // 只复用**带 caption 的**新方案。老资产里只有 elements,那正是"只动光影"的旧提示词,
        // 拿它继续出片等于把已经修掉的毛病又跑一遍;视觉模型免费,重新看一次更划算。
        Some(_) if source.motion_plan.has_caption() => source.motion_plan.clone(),
        Some(chat) => {
            // 看图这一步是**免费**的,但它偶尔会超时/返回空方案。以前空方案会悄悄退回
            // 通用提示词,然后照样提交那条要花 1 元的视频 —— 用户拿到的是一段没有
            // 元素运动的片子,还不知道钱花在了哪儿。这里先免费重试一次;仍然空就直接
            // 报错,不提交视频。
            let first = chat.plan_motion_vision(&source, &image_bytes);
            if !first.is_empty() {
                first
            } else {
                tracing::warn!("第一次看图没认出会动的实体,免费重试一次");
                let second = chat.plan_motion_vision(&source, &image_bytes);
                if second.is_empty() {
                    anyhow::bail!(
                        "这次没从画面里认出会动的实体,先不提交视频(避免白花 1 元)。稍后再点一次「生成动图」。"
                    );
                }
                second
            }
        }
        None => mcwp_spec::MotionPlan::default(),
    };
    // 方案**写回原图那一份**:它的主人是这张图(见 `photos::save_motion_plan`)。
    // 只在这张图还没有方案时写 —— 库里已经有一份就说明是"拿来用"的那条路,不用重复写。
    if !plan.is_empty() && !source.motion_plan.has_caption() {
        if let Err(err) = photos::save_motion_plan(entry, &plan) {
            // 写不进去不该让整条链路失败:方案已经在内存里,片子照样能做
            tracing::warn!("动效方案没写回库里那张({err:#})");
        }
    }
    // 3 · **默认单图首帧**。让模型自己找实体运动;首尾帧引导只在
    //     `GUIDED_FRAMES_ENABLED` 打开时才生成局部位移尾帧。实测首尾帧会把运动
    //     压到光流 0.06 左右,单图 quality/speed 分别是 2.92/8.79。
    //     播放器仍在接缝处淡回首帧(见 `LOOP_FADE_SECS`)。
    //
    // 3.5 · 提交前把图收拾成"服务商自己推得动"的比例:它按"短边 1080 × 图片比例、向下取整"推
    //       成片尺寸,推出奇数长边时 H.264 编不出来。**这不是 2026-10-16 那几次 FAIL 的病根**
    //       (裁成 1920×1080 之后照样失败,见 `mcwp_ai::VIDEO_SIZES` 的更正),它只是一份便宜的
    //       保险:比值本来就安全的图**原样发**(不重编码 —— 这是条件图,糊了整段都糊),不安全的才裁。
    let mut bitmap = mcwp_image::Bitmap::decode(&image_bytes)
        .with_context(|| format!("解不开这张图:{}", entry.image.display()))?;
    let mut reencoded = false;
    if let Some((target_width, target_height)) =
        mcwp_ai::video_image_fix((source.width, source.height))
    {
        let original = (bitmap.width, bitmap.height);
        let derived = mcwp_ai::derived_video_size(original);
        bitmap = bitmap.crop_to_ratio(target_width, target_height);
        reencoded = true;
        tracing::info!(
            "条件图裁成 {}×{}(原 {}×{};不裁的话服务商会推 {}×{},长边奇数就 FAIL) · 目标比例 {}:{}",
            bitmap.width,
            bitmap.height,
            original.0,
            original.1,
            derived.0,
            derived.1,
            target_width,
            target_height
        );
    }
    let submit_bytes = if reencoded {
        bitmap.encode_jpeg(92)?
    } else {
        image_bytes.clone()
    };
    let image_url = mcwp_ai::image_data_url(&submit_bytes);
    let (guide, guide_warps, max_shift) = if GUIDED_FRAMES_ENABLED {
        let (guide, warps, max_shift) = motion_guide_bitmap(&bitmap, &plan);
        (Some(guide), warps, max_shift)
    } else {
        (None, 0, 0.0)
    };
    // 2 · 拼提示词。默认是单图版本;只有显式打开首尾帧引导时才用
    //     `video_prompt_for_frames`。没配对话模型也要能跑:方案为空时退化成
    //     通用描述(见 `fallback_motion_prompt`)。
    let motion = if guide_warps > 0 {
        mcwp_ai::video_prompt_for_frames(&plan)
    } else if plan.is_empty() {
        match chat {
            Some(chat) => chat.plan_motion(&source),
            None => mcwp_ai::fallback_motion_prompt().to_string(),
        }
    } else {
        mcwp_ai::video_prompt(&plan)
    };
    tracing::info!(
        "这一轮的动效提示词({}):{motion}",
        if guide_warps > 0 {
            "首尾帧"
        } else if plan.is_empty() {
            "通用"
        } else if plan.has_caption() {
            "caption"
        } else {
            "元素级兜底"
        }
    );
    let generated = match guide.as_ref() {
        Some(guide) if guide_warps > 0 => {
            tracing::info!(
                "动效尾帧:{} 个局部引导区 · 最大位移 {:.1}px · 构图与首帧一致",
                guide_warps,
                max_shift
            );
            let guide_url = mcwp_ai::image_data_url(&guide.encode_jpeg(92)?);
            videos.animate_frames(&image_url, &guide_url, &motion, screen)?
        }
        _ => videos.animate(&image_url, &motion, screen)?,
    };
    video_payload_from_bytes(&source, plan, generated.bytes, screen)
}

/// 把一段**已经拿到手的**成片收拾成入库载荷:解出第一帧当封面,取尺寸与时长。
///
/// 单独一个函数,是因为成片有**两个来源**:刚提交回来的那条(`animate_photo`),
/// 以及照任务号捡回来的那条(`--video-fetch`)。超时丢掉的任务服务端还在跑、钱也已经花了,
/// 捡回来这条必须走**同一套落地逻辑** —— 少一个封面,库里那张条目就是个死链。
fn video_payload_from_bytes(
    source: &Photo,
    plan: mcwp_spec::MotionPlan,
    bytes: Vec<u8>,
    screen: (u32, u32),
) -> Result<VideoPayload> {
    let temp = std::env::temp_dir().join(format!("mcwp-animate-{}.mp4", source.id));
    std::fs::write(&temp, &bytes).with_context(|| format!("写临时视频失败:{}", temp.display()))?;
    let decoded = mcwp_video::Player::open(&temp, screen).and_then(|mut player| {
        let info = player.info();
        // 封面和播放器用同一帧:片头从黑场淡入时,封面不能是一张黑图。
        let (pixels, _) = first_display_frame(&mut player, info.duration.as_secs_f32())
            .context("这段视频一帧都解不出来")?;
        // 顺手把**整段解一遍**,量出末尾比开头亮多少。它只在这里量一次,多花几秒,
        // 而这一步本来就在等几分钟的出片;换来的播放侧每帧都能拿它把曝光拉回一致。
        let first = pixels.clone();
        let mut last = first.clone();
        while let Some(frame) = player.next_frame()? {
            last = frame.to_vec();
        }
        Ok((info, pixels, seam_gain_for(&first, &last)))
    });
    // 解完就删。失败也删 —— 中间产物留在盘上,下次"文件被占着"的怪事就说不清了。
    let _ = std::fs::remove_file(&temp);
    let (info, pixels, seam_gain) =
        decoded.context("新视频解不出第一帧(这台机器的解码器没装上?)")?;

    let cover = mcwp_image::Bitmap {
        width: info.width,
        height: info.height,
        rgba: pixels,
    };
    let created = now_secs();
    let mut photo = Photo {
        title: source.title.clone(),
        prompt: source.prompt.clone(),
        // 这张片子**是从哪儿来的**:面板要靠它把"会话里那条回复配的静图"和"随后做出来的
        // 这段片子"接起来(见 `Photo::origin_id`)。`--video-fetch` 那条路也走这个函数,
        // 所以照任务号捡回来的那一条同样记得住出处。
        origin_id: source.id.clone(),
        // 记**解码出来的**尺寸:画面按 cover 裁,而视频和原图的尺寸不一定一样
        width: info.width,
        height: info.height,
        kind: mcwp_spec::photo::PhotoKind::Video,
        duration_ms: info.duration.as_millis() as u32,
        // 动效配方照抄原图那份:视频这条路上它用不上(见 `PhotoParams::from_video`),
        // 但哪天解码器不可用、只能把封面当静图放时,它还派得上用场。
        motion: source.motion,
        palette: mcwp_spec::palette_from_rgba(&cover.rgba, cover.width, cover.height),
        seed: (created & 0xffff_ffff) as u32,
        created,
        // 方案**跟着视频一起入库**:下次想再做一遍(换个模型、或者只是重试)时,
        // 不用再去看一遍图;而"这张动效是怎么来的"也留了个痕迹。
        motion_plan: plan,
        seam_gain,
        ..Photo::default()
    };
    // **id 必须重新生成**:接着用原图那个 id 的话,这条视频的封面(`<id>.png`)
    // 会把原图那张画面文件盖掉 —— 库里就再也找不到那张静图了。
    photo.normalize();
    Ok(VideoPayload {
        photo,
        mp4: bytes,
        cover,
    })
}

/// 一段刚出炉的视频落地:入库 → 上屏。
///
/// 和 `adopt_photo` 的顺序**不一样**,这是被解码器逼的:播放器是从**文件**读的,
/// 所以必须先有 mp4 落在盘上才有得放。补偿的办法是把"写不进库"这条退路留在手上 ——
/// 写不进去就写 `%TEMP%`,用户等了十分钟,不能因为库目录只读就什么都不给。
fn adopt_video(
    renderer: &mut Renderer,
    library: &mut Vec<LibraryEntry>,
    payload: VideoPayload,
    screen: (u32, u32),
    brightness: f32,
) -> Result<ActivePhoto> {
    let VideoPayload { photo, mp4, cover } = payload;
    match photos::save_video(&photo, &mp4, &cover) {
        Ok(entry) => {
            tracing::info!("新动态壁纸已入库:{}", entry.meta.display());
            *library = scan_library();
            let path = entry.video.clone().unwrap_or_else(|| entry.image.clone());
            show_video(renderer, entry.photo.clone(), &path, screen, brightness)
        }
        Err(err) => {
            tracing::warn!("动态壁纸入库失败(照常上屏):{err:?}");
            let path = std::env::temp_dir().join(format!("mcwp-animate-{}.mp4", photo.id));
            std::fs::write(&path, &mp4)
                .with_context(|| format!("连临时视频都写不下:{}", path.display()))?;
            show_video(renderer, photo, &path, screen, brightness)
        }
    }
}

/// 从库里换一张(点卡片 / 托盘"换一张"都走这里)。
///
/// 解码是在上屏这条路上**同步**做的:1080p 一次约 200 ms,对"用户点了一下"这个动作
/// 可以接受;换成后台预取要引入一整套"图还没到该显示什么"的状态,此刻不值当。
fn switch_to_photo(
    renderer: &mut Renderer,
    library: &[LibraryEntry],
    index: usize,
    screen: (u32, u32),
    brightness: f32,
) -> Result<ActivePhoto> {
    let entry = library
        .get(index)
        .ok_or_else(|| anyhow::anyhow!("壁纸库里没有第 {index} 张"))?;
    // 视频条目交给播放器。它那张 `<id>.png` 只是封面(第一帧),
    // 当成静图放也能看,但那正是"点了动态壁纸却不动"的那种 bug。
    if let Some(path) = entry.source.video.clone() {
        let photo = entry.source.photo.clone();
        return show_video(renderer, photo, &path, screen, brightness);
    }
    let bitmap = entry
        .source
        .read_image()
        .with_context(|| format!("解不开 {}", entry.source.image.display()))?;
    show_photo(
        renderer,
        entry.source.photo.clone(),
        &bitmap,
        screen,
        brightness,
    )
}

/// 一张刚出炉的照片落地:入库 → 上屏。
///
/// 顺序是"**先上屏再入库**":入库可能失败(盘满、权限、路径被占),而**上屏不该因此被挡住** ——
/// 用户要看的是那张图,库里少一条只是少一条,下次还能再生成。
fn adopt_photo(
    renderer: &mut Renderer,
    library: &mut Vec<LibraryEntry>,
    payload: PhotoPayload,
    screen: (u32, u32),
    brightness: f32,
) -> Result<ActivePhoto> {
    let active = show_photo(renderer, payload.photo, &payload.bitmap, screen, brightness)?;
    match photos::save(&active.photo, &payload.bytes, &payload.bitmap) {
        Ok(entry) => {
            tracing::info!("新壁纸已入库:{}", entry.meta.display());
            *library = scan_library();
        }
        Err(err) => tracing::warn!("入库失败(壁纸照常显示):{err:?}"),
    }
    Ok(active)
}

/// `save_generated_photo` 的结果。
struct SavedShot {
    /// 这张在库里的 id(面板拿它去贴预览图)
    id: String,
    title: String,
    /// 库没写进去、退回到「直接挂上」了吗(那时桌面已经换过了)
    applied_as_fallback: bool,
}

/// 把刚画出来的那张**只入库、不上屏**。生图那条路的落地点。
///
/// 为什么不像 `adopt_photo` 那样顺手换上:「这回画出来的要不要用」是用户的决定,自动换掉桌面
/// 等于替他做了决定,还会把他正看着的那张顶下去。用户的原话是:**"生成完不要自动设为壁纸,
/// 需要放进壁纸库"** —— 想用哪张由他自己说:右键那条回复下面的预览图 →「设为壁纸」。
///
/// **入库失败是唯一的例外**:那一刻这张图只存在于内存里,什么都不做等于让用户白等十几秒,
/// 所以退回老行为(照常挂上)并在状态栏说清楚。库写不进去(只读盘、盘满)本来就罕见,
/// 真碰上时"图还在"比"库里干净"重要。
fn save_generated_photo(
    renderer: &mut Renderer,
    library: &mut Vec<LibraryEntry>,
    payload: PhotoPayload,
    screen: (u32, u32),
    brightness: f32,
) -> Result<SavedShot> {
    match photos::save(&payload.photo, &payload.bytes, &payload.bitmap) {
        Ok(entry) => {
            tracing::info!("新壁纸已入库(不自动换上):{}", entry.meta.display());
            *library = scan_library();
            Ok(SavedShot {
                id: entry.photo.id.clone(),
                title: entry.photo.title.clone(),
                applied_as_fallback: false,
            })
        }
        Err(err) => {
            tracing::warn!("入库失败({err:?}),退回「直接挂上」—— 不然这张图就白画了");
            let title = payload.photo.title.clone();
            let stored = adopt_photo(renderer, library, payload, screen, brightness)?;
            Ok(SavedShot {
                id: stored.photo.id.clone(),
                title,
                applied_as_fallback: true,
            })
        }
    }
}

/// 抬进来的壁纸在库里的 id:**按源文件路径**定,不按时间。
///
/// 为什么:同一张"桌面壁纸"会被反复抬 —— 点两次「用当前壁纸」、`--adopt` 又跑一遍、
/// 或者换过桌面壁纸之后再抬一次。按时间生成 id 的话每次都是一条新记录,库很快就堆成一串
/// 长得一模一样的副本(实测踩过:调试时点了八下,库里就多了八张)。
/// 按路径定就稳定了:撞上同一个 id 就替换,库里那条始终代表"桌面上现在那张"。
///
/// 前缀 `adopt` 让它一眼能和生图那种纯十六进制的 id 区分开;路径先转小写,因为 Windows 的
/// 文件名本来就不区分大小写,`C:\A.jpg` 和 `c:\a.jpg` 不该算成两张。
fn adopted_id(path: &std::path::Path) -> String {
    let digest = mcwp_spec::fnv1a64(path.to_string_lossy().to_lowercase().as_bytes());
    format!("adopt{digest:016x}")
}

/// 抬进来的那张壁纸叫什么。
///
/// 取文件名主干,但要认得出 Windows 那个转码件:用户从「设置」里选壁纸时,系统把原图转码成
/// 一份**没有扩展名**的 `…\Themes\TranscodedWallpaper`,而 `SPI_GETDESKWALLPAPER` 给回来的
/// 正是它 —— 照搬主干的话,库里每一张抬进来的壁纸都会叫「TranscodedWallpaper」。
fn desktop_wallpaper_title(path: &std::path::Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default()
        .trim();
    // 没有扩展名 = 就是那个转码件;主干恰好叫 TranscodedWallpaper 也算(有些机器带扩展名)。
    if path.extension().is_none() || stem.eq_ignore_ascii_case("TranscodedWallpaper") {
        return "桌面壁纸".to_string();
    }
    if stem.is_empty() {
        return "桌面壁纸".to_string();
    }
    stem.to_string()
}

/// 把**桌面上现在那张壁纸**抬进来:读路径 → 解码 → 入库 → 上屏。
///
/// 为什么值得有这条路:生图要花钱、要等十几秒,而用户机器上本来就压着一张静态壁纸 ——
/// 把那张图直接抬进渲染管线,当场就得到一张「能动的」壁纸(静帧呼吸 + Ken Burns 推拉),
/// 不必先等模型。它同时也是**库空着时的兜底**:我们的窗口是盖在桌面上的,什么都不画就是
/// 一屏占位色,而"把用户原来那张抬上来"看不出接缝。
///
/// 抬进来的那张会**入库**,所以它和生图出来的壁纸一样能反复切换、也能被删掉。
/// 动效给的是 `MotionRecipe::default()`(静帧呼吸:推近 12%、速度 30%)—— 保守,但一眼看得出在动。
fn adopt_desktop(
    renderer: &mut Renderer,
    library: &mut Vec<LibraryEntry>,
    screen: (u32, u32),
    brightness: f32,
) -> Result<ActivePhoto> {
    let path = mcwp_platform::wallpaper::current_image_path().ok_or_else(|| {
        anyhow::anyhow!("系统说桌面上现在没有图片(纯色背景 / 幻灯片 / 被策略接管)")
    })?;
    let bytes =
        std::fs::read(&path).with_context(|| format!("读不到桌面壁纸文件:{}", path.display()))?;
    let bitmap = mcwp_image::Bitmap::decode(&bytes)
        .with_context(|| format!("桌面壁纸解不开(不是 PNG / JPEG?):{}", path.display()))?;

    // 4K/8K 的壁纸可能大过这块 GPU 的纹理上限,那就**就地缩一份只用来上屏的** ——
    // 落盘的仍然是原图,`photo.width/height` 也仍然记原图尺寸(cover 要按它算)。
    let (original_width, original_height) = (bitmap.width, bitmap.height);
    let limit = renderer.max_texture_size();
    let bitmap = if original_width > limit || original_height > limit {
        let small = bitmap.thumbnail(limit, limit);
        tracing::info!(
            "桌面壁纸 {original_width}×{original_height} 超过 GPU 纹理上限 {limit},上屏前缩到 {}×{}",
            small.width,
            small.height
        );
        small
    } else {
        bitmap
    };

    // 反复抬同一张不该在库里堆副本:先按 id 找找有没有旧的那条,有就删掉再存。
    // **删掉而不是直接覆盖**:画面文件的扩展名可能变(jpg → png),`save` 只写新的那份,
    // 旧的那份就成了一块谁也认不出来的孤儿(扫库只认 `.json`,而它已经指向新文件了)。
    let id = adopted_id(&path);
    if let Some(old) = library.iter().find(|entry| entry.source.photo.id == id) {
        if let Err(err) = photos::remove(&old.source) {
            tracing::warn!("替换旧的「抬进来」记录时删不掉旧文件:{err:?}");
        }
    }

    let created = now_secs();
    let mut photo = Photo {
        id,
        // 标题取文件名:`…\冬日雪山.jpg` → 「冬日雪山」;转码件那种没名没姓的给「桌面壁纸」。
        title: desktop_wallpaper_title(&path),
        // 提示词**故意留空**:这张不是"画"出来的,没有提示词可写。空着比编一句假的好 ——
        // 库里一眼就能看出哪几张是抬进来的、哪几张是生成的。
        width: original_width,
        height: original_height,
        // 动效种类是"静帧呼吸"(`MotionKind` 的默认值):只推拉,不往上叠雪雨雾 ——
        // 往一张不认识的风景照上叠雪,十有八九不合适,而它是用户自己的壁纸,错了很扎眼。
        motion: mcwp_spec::MotionRecipe::default(),
        palette: mcwp_spec::palette_from_rgba(&bitmap.rgba, bitmap.width, bitmap.height),
        seed: (created & 0xffff_ffff) as u32,
        created,
        ..Photo::default()
    };
    photo.normalize();
    adopt_photo(
        renderer,
        library,
        PhotoPayload {
            reply: String::new(),
            photo,
            bytes,
            bitmap,
            // 这两项如实填:这条路**根本不需要模型**,不是"模型没接上"的降级
            degraded: false,
            watermarked: false,
            calls: 0,
        },
        screen,
        brightness,
    )
}

/// 导入那一路的标题:文件名(去掉扩展名)。名字取不出来就给一句「导入的图片」。
///
/// 和「抬桌面壁纸」那份(`desktop_wallpaper_title`)的区别只在兜底:那条路的路径可能是
/// 系统转码件(没有名字),这条路是用户自己挑的文件,基本总有名字。
fn imported_title(path: &std::path::Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().trim().to_string())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "导入的图片".to_string())
}

/// 把**用户从文件里挑的那张图**收进来:解码 → 入库 → 上屏。
///
/// 和「抬桌面壁纸」几乎一样,只差两点:
/// - 路径是对话框给的,不是系统说"桌面现在挂着哪张";
/// - id 复用了 `adopted_id`(按路径算),所以**同一张图反复导入只会有一条** ——
///   用户在资源管理器里改完图再导一次,库里换掉的是同一条记录,不会堆出一串副本。
///
/// 落盘的仍是**文件原来的字节**(不重新编码,和生图那条路一个规矩)。
fn import_photo(
    renderer: &mut Renderer,
    library: &mut Vec<LibraryEntry>,
    path: &std::path::Path,
    screen: (u32, u32),
    brightness: f32,
) -> Result<ActivePhoto> {
    let bytes =
        std::fs::read(path).with_context(|| format!("读不到这个文件:{}", path.display()))?;
    let bitmap = mcwp_image::Bitmap::decode(&bytes)
        .with_context(|| format!("这张图解不开(只认 PNG / JPEG):{}", path.display()))?;

    // 大过纹理上限的就地缩一份只用来上屏的(落盘的还是原图,元数据也记原尺寸)——
    // 和抬桌面壁纸一样,4K 以上的图在这块核显上会直接建不出纹理。
    let (original_width, original_height) = (bitmap.width, bitmap.height);
    let limit = renderer.max_texture_size();
    let bitmap = if original_width > limit || original_height > limit {
        let small = bitmap.thumbnail(limit, limit);
        tracing::info!(
            "导入的图 {original_width}×{original_height} 超过 GPU 纹理上限 {limit},上屏前缩到 {}×{}",
            small.width,
            small.height
        );
        small
    } else {
        bitmap
    };

    let id = adopted_id(path);
    if let Some(old) = library.iter().find(|entry| entry.source.photo.id == id) {
        if let Err(err) = photos::remove(&old.source) {
            tracing::warn!("替换旧的导入记录时删不掉旧文件:{err:?}");
        }
    }

    let created = now_secs();
    let mut photo = Photo {
        id,
        title: imported_title(path),
        // 提示词留空:这张不是"画"出来的。空着比编一句假的好 —— 库里一眼能看出哪几张是
        // 生成、哪几张是抬进来或导入的(和「抬桌面壁纸」同一个态度)。
        width: original_width,
        height: original_height,
        // 静帧呼吸(推拉),不往上叠雪雨雾:用户自己的照片叠什么都可能不合适。
        motion: mcwp_spec::MotionRecipe::default(),
        palette: mcwp_spec::palette_from_rgba(&bitmap.rgba, bitmap.width, bitmap.height),
        seed: (created & 0xffff_ffff) as u32,
        created,
        ..Photo::default()
    };
    photo.normalize();
    adopt_photo(
        renderer,
        library,
        PhotoPayload {
            reply: String::new(),
            photo,
            bytes,
            bitmap,
            degraded: false,
            watermarked: false,
            calls: 0,
        },
        screen,
        brightness,
    )
}

/// 设置页要显示的模型配置:盘上那份(空字段已经用内置默认补上)+ 有没有环境变量在盖它。
fn ai_config_state() -> mcwp_ui::AiConfigState {
    let stored = mcwp_ai::Config::stored();
    mcwp_ui::AiConfigState {
        base_url: stored.base_url.clone(),
        model: stored.model.clone(),
        api_key: stored.api_key.clone(),
        // 生图那几个**原样给**(`Stored` 里是空串):空串是合法的,意思是"跟着上面那组走"。
        // 不能提前填上默认值 —— 那样用户就分不清"我自己填的"和"程序替我填的",
        // 想改回"跟对话同一家"也就没有表达方式了。
        image_base_url: stored.image_base_url,
        image_model: stored.image_model,
        image_api_key: stored.image_api_key,
        image_quality: stored.image_quality,
        image_watermark: stored
            .image_watermark
            .unwrap_or(mcwp_ai::DEFAULT_IMAGE_WATERMARK),
        // 同理原样给:空串 = 用内置默认(`cogvideox-3`)
        video_model: stored.video_model,
        thinking: mcwp_ui::ThinkingChoice::from_option(stored.thinking),
        env_override: mcwp_ai::Config::env_override(),
    }
}

/// 设置页点了「保存」:落盘 + **就地换掉客户端**,返回一行状态给面板。
///
/// 就地重建是重点 —— 不重建的话用户得重启程序才生效,而"配完还得重启"是这类设置页最容易挨骂的
/// 地方。写盘失败时原来的客户端原样留着:配置没写成,能力也不该跟着消失。
fn save_ai_config(
    stored: &mcwp_ai::Stored,
    ai: &mut Option<mcwp_ai::Client>,
    images: &mut Option<mcwp_ai::Images>,
    videos: &mut Option<mcwp_ai::Videos>,
) -> String {
    if let Err(err) = mcwp_ai::Config::save(stored) {
        tracing::error!("写 ai.json 失败:{err:?}");
        return format!("保存失败:{err}");
    }
    // 两个客户端一起按新配置重建。分开重建的话,"改的往往就是同一把 key"这件事会让用户
    // 觉得"生图要重启才生效" —— 而这类设置页最容易挨骂的就是"配完还得重启"。
    let describe = rebuild_clients(ai, images, videos);
    if mcwp_ai::Config::env_override() {
        // 环境变量优先级更高:写得进去,但不生效。不说明白,用户只会觉得"保存没反应"。
        return "已写入 ai.json · 但环境变量在生效,实际用的是它们".to_string();
    }
    describe
}

/// 按盘上(已与环境变量合并)的配置重建两个客户端,返回一行给面板看的状态。
///
/// 启动时和设置页保存后走的都是它 —— 两条路各写一遍的话,迟早出现"启动时认某个新字段、
/// 保存后不认"这种只在某个时序下才发作的怪事。
fn rebuild_clients(
    ai: &mut Option<mcwp_ai::Client>,
    images: &mut Option<mcwp_ai::Images>,
    videos: &mut Option<mcwp_ai::Videos>,
) -> String {
    *ai = match mcwp_ai::Config::load() {
        Ok(cfg) => {
            let client = mcwp_ai::Client::new(cfg);
            tracing::info!("对话模型:{}", client.describe());
            Some(client)
        }
        Err(err) => {
            // 没配只是"少个能力",不是错误:面板照常能开、能聊,回复位会说清楚去哪儿配
            tracing::warn!("没配对话模型:{err}");
            None
        }
    };
    *images = match mcwp_ai::ImagesConfig::load() {
        Ok(cfg) => {
            let painter = mcwp_ai::Images::new(cfg);
            tracing::info!("生图:{}", painter.describe());
            Some(painter)
        }
        Err(err) => {
            tracing::warn!("没配生图:{err}");
            None
        }
    };
    *videos = match mcwp_ai::VideosConfig::load() {
        Ok(cfg) => {
            let animator = mcwp_ai::Videos::new(cfg);
            tracing::info!("图生视频:{}", animator.describe());
            Some(animator)
        }
        Err(err) => {
            tracing::warn!("没配图生视频:{err}");
            None
        }
    };
    match ai {
        Some(client) => format!("已保存 · {}", client.describe()),
        None => "已保存 · 还没填 API Key".to_string(),
    }
}

/// 把内部错误压成一句能显示在面板上的话(按**字符**截,中文按字节截会切出半个字)。
fn brief(reason: &str) -> String {
    let flat: String = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 90 {
        format!("{}…", flat.chars().take(90).collect::<String>())
    } else {
        flat
    }
}

/// 面板窗口的标题。要和 `panel.slint` 的 `title` 一字不差 —— 单实例守卫靠它找窗口。
const PANEL_TITLE: PCWSTR = w!("MC 壁纸 · 控制面板");

/// 单实例守卫:已经有进程在挂壁纸时返回 `false`,让本次启动直接收摊。
///
/// 桌面快捷方式很容易被连点两下。第二个进程要是照常跑,就会再建一个宿主窗口去抢同一个
/// WorkerW,桌面直接花掉,而且它没有任何可见反馈。所以第二个进程只做一件事:把第一个
/// 进程的控制面板叫到前台(跨进程找窗口就行,不需要 IPC),然后退出。
///
/// 「列目录 / 导出预设 / 排查用参数」不挂桌面,调用方别把它们卡在这里。
fn claim_single_instance() -> bool {
    let name = w!("Local\\MCWP.Wallpaper.Singleton");
    // 句柄故意不关:它活到进程退出,互斥体就一直占着,下一个进程才看得见。
    let acquired = unsafe {
        SetLastError(ERROR_SUCCESS);
        match CreateMutexW(None, true, name) {
            Err(err) => {
                tracing::warn!("创建单实例互斥体失败({err}),照常启动");
                true
            }
            Ok(_) => GetLastError() != ERROR_ALREADY_EXISTS,
        }
    };
    if acquired {
        return true;
    }

    if raise_panel() {
        tracing::info!("MC 壁纸已在运行,已把控制面板叫到前台,本次不再启动");
    } else {
        tracing::info!("MC 壁纸已在运行(当前没开控制面板),本次不再启动");
    }
    false
}

/// 把已经打开的控制面板叫到最前。面板没开(窗口不存在)时返回 `false`。
///
/// 跨线程/跨进程都靠窗口标题找,所以不需要给面板加"请到前台"的接口。
fn raise_panel() -> bool {
    match unsafe { FindWindowW(PCWSTR::null(), PANEL_TITLE) } {
        Ok(hwnd) => {
            unsafe {
                let _ = ShowWindow(hwnd, SW_RESTORE);
                let _ = SetForegroundWindow(hwnd);
            }
            true
        }
        Err(_) => false,
    }
}

/// 打印窗口层级(仅 `dev-tools`)。产品构建里这一步是空操作。
fn trace_chain(_hwnd: windows::Win32::Foundation::HWND, _label: &str) {
    #[cfg(feature = "dev-tools")]
    mcwp_platform::diag::describe_chain(_hwnd, _label);
}

/// `--plan <一句话>`:只跑**提示词那一步**,把发给模型的请求、和模型给的计划原样打出来。
///
/// 为什么不直接用 `--images-selftest`:那个要跑完整条链路(等图十几秒、还要落一份预览文件),
/// 而"画歪了"最常见的坏法恰恰是**提示词先歪了** —— 调提示词时这里一次调用只要几秒钟,
/// 不建窗口、不出图、不进库。`--plan-current <库 id>` 能把"现在屏幕上那张"一并带上,
/// 复现面板里的上下文(同一个提示词,带不带当前壁纸,给出来的东西可能完全不同)。
fn run_plan_selftest(request: &str, current_id: Option<&str>) -> Result<()> {
    let chat = mcwp_ai::Client::new(mcwp_ai::Config::load()?);
    let current = match current_id {
        Some(id) => Some(
            photos::load(id)
                .with_context(|| format!("库里找不到这张:{id}"))?
                .photo,
        ),
        None => None,
    };

    println!("对话:{}", chat.describe());
    println!("用户说:{request}");
    println!(
        "当前壁纸:{}",
        current
            .as_ref()
            .map(|photo| photo.title.as_str())
            .unwrap_or("(没有 · 和面板第一次打开时一样)")
    );
    println!("\n---- 发给模型的用户消息 ----");
    println!("{}", mcwp_ai::photo_request(request, current.as_ref()));

    println!("\n---- 模型给的计划 ----");
    let started = Instant::now();
    let plan = chat.plan_photo(request, current.as_ref());
    println!(
        "耗时:{:.1}s · 调用 {} 次 · 降级:{}",
        started.elapsed().as_secs_f32(),
        plan.calls,
        if plan.degraded {
            "是(退化成拿原话去画)"
        } else {
            "否"
        }
    );
    println!("回复:{}", plan.reply);
    println!("标题:{}", plan.title);
    println!("提示词:{}", plan.image_prompt);
    println!("动效:{}", plan.motion.summary());
    Ok(())
}

/// `--plan-motion <库 id>`:只跑**看图写动效**那一步,把元素级方案和最终那句提示词摊开。
///
/// 为什么必须有这个入口:这一步之后的**图生视频要花钱**(1 元/次,见 `mcwp_ai::Videos`),
/// 而"画面怎么动"这件事得反复调 —— 元素选没选对、动作写得像不像话,看方案一眼就知道。
/// 这一步自己(视觉模型)是免费的,所以调提示词可以随便跑。
fn run_motion_selftest(id: &str) -> Result<()> {
    let chat = mcwp_ai::Client::new(mcwp_ai::Config::load()?);
    let entry = photos::load(id).with_context(|| format!("库里找不到这张:{id}"))?;
    let bytes = std::fs::read(&entry.image)
        .with_context(|| format!("读不出这张图:{}", entry.image.display()))?;
    println!("对话:{}", chat.describe());
    println!("看图:{}", chat.vision_model());
    println!("这张:{}", entry.photo.title);
    println!("生成它那句话:{}", entry.photo.prompt);

    println!("\n---- 分析出的元素级方案 ----");
    let started = Instant::now();
    let plan = chat.plan_motion_vision(&entry.photo, &bytes);
    println!("耗时:{:.1}s", started.elapsed().as_secs_f32());
    println!("{}", serde_json::to_string_pretty(&plan)?);

    // 默认出片走**单图首帧**这条路,所以这里展示单图版本;首尾帧只在显式打开
    // `GUIDED_FRAMES_ENABLED` 时使用。
    let prompt = mcwp_ai::video_prompt(&plan);
    println!(
        "\n---- 单图出片实际会发的提示词({} 字)----",
        prompt.chars().count()
    );
    println!("{prompt}");
    println!(
        "\n(真要出片还得花 1 元/次:「{}」+ 单图首帧 + quality 档 + 10 秒)",
        mcwp_ai::DEFAULT_VIDEO_MODEL
    );
    Ok(())
}

/// `--animate-save <库 id>`:`--animate` 的**不打扰桌面**版 —— 跑完那条花钱的链路,
/// 落进壁纸库,但**不上屏、不建窗口**。
///
/// 为什么值得单独留一个入口:出片一次 1 元、还要等几分钟,而验证它的时候最怕两件事 ——
/// ① 验证本身把用户的桌面换掉;② 和正在跑的面板抢"单实例"那把锁(抢不到就什么都不做,
/// 白等一场还看不出来为什么)。这条路在后台跑、产物进库,想换上去在面板里点一下就行。
fn run_animate_save(id: &str, screen: (u32, u32)) -> Result<()> {
    let chat = mcwp_ai::Client::new(mcwp_ai::Config::load()?);
    let videos = mcwp_ai::Videos::new(mcwp_ai::VideosConfig::load()?);
    let entry = photos::load(id).with_context(|| format!("库里找不到这张:{id}"))?;
    println!("对话:{}", chat.describe());
    println!("图生视频:{}", videos.describe());
    println!("这张:{}", entry.photo.title);
    let started = Instant::now();
    let payload = animate_photo(Some(&chat), &videos, &entry, screen)?;
    let saved = photos::save_video(&payload.photo, &payload.mp4, &payload.cover)?;
    println!(
        "出片并入库({:.0}s):「{}」{} 毫秒 · {:.1} MB",
        started.elapsed().as_secs_f32(),
        saved.photo.title,
        saved.photo.duration_ms,
        payload.mp4.len() as f32 / 1_048_576.0,
    );
    println!("落点:{}", saved.meta.display());
    Ok(())
}

/// 照任务号取回一条**已经提交过**的视频,并做成入库载荷。
///
/// 它是 CLI 的 `--video-fetch` 和面板上那颗「免费取回」共用的那一段:
/// **不提交、不再花钱**,只做"查询 + 下载 + 解封面"。
fn fetch_video_payload(
    videos: &mcwp_ai::Videos,
    entry: &photos::Entry,
    task_id: &str,
    screen: (u32, u32),
) -> Result<VideoPayload> {
    let generated = videos.fetch(task_id)?;
    // 动效方案沿用库里存的那份(分析过就有,没有就是空方案):取回一条已经算好的视频,
    // 不该再花掉一次看图 —— 何况这一次的成片早就生成了,再看图也改不了它。
    video_payload_from_bytes(
        &entry.photo,
        entry.photo.motion_plan.clone(),
        generated.bytes,
        screen,
    )
}

/// `--video-fetch <任务号> <库 id>`:把一条**已经提交过**的视频取回来入库。
///
/// 为什么要留这么一条路:出片那一步是花钱的(1 元/次),而服务商的排队能从 1 分钟
/// 拉到十几分钟 —— 客户端不可能一直等着,只能超时收手,那 1 元就悬在半空:
/// 服务端还在跑,结果没人取。任务号在日志和报错里都留着,这条路负责照号捡回来。
fn run_video_fetch(task_id: &str, photo_id: &str, screen: (u32, u32)) -> Result<()> {
    let videos = mcwp_ai::Videos::new(mcwp_ai::VideosConfig::load()?);
    let entry = photos::load(photo_id).with_context(|| format!("库里找不到这张:{photo_id}"))?;
    println!("图生视频:{}", videos.describe());
    println!("源图:{}", entry.photo.title);
    println!("任务号:{task_id}");
    let started = Instant::now();
    let payload = fetch_video_payload(&videos, &entry, task_id, screen)?;
    let saved = photos::save_video(&payload.photo, &payload.mp4, &payload.cover)?;
    println!(
        "取回并入库({:.0}s):「{}」{} 毫秒 · {:.1} MB",
        started.elapsed().as_secs_f32(),
        saved.photo.title,
        saved.photo.duration_ms,
        payload.mp4.len() as f32 / 1_048_576.0,
    );
    println!("落点:{}", saved.meta.display());
    Ok(())
}

/// `--images-selftest`:把整条生图链路跑一遍,原样摊开。
///
/// 和 `--plan` 是一对:那个看"提示词那一步",这个看"图有没有出来"。
/// 两层都打出来,是因为出图难看时你得先知道是提示词写歪了、还是模型画歪了。
fn run_images_selftest(request: &str, screen: (u32, u32)) -> Result<()> {
    let chat = mcwp_ai::Client::new(mcwp_ai::Config::load()?);
    let images = mcwp_ai::Images::new(mcwp_ai::ImagesConfig::load()?);
    println!("对话:{}", chat.describe());
    println!("生图:{}", images.describe());
    println!("用户说:{request}");
    let started = Instant::now();
    let payload = generate_photo(&chat, &images, request, None, screen)?;
    println!(
        "耗时:{:.1}s  提示词调用:{} 次  降级:{}",
        started.elapsed().as_secs_f32(),
        payload.calls,
        if payload.degraded {
            "是(拿原话去画的)"
        } else {
            "否"
        }
    );
    println!("回复:{}", payload.reply);
    println!("标题:{}", payload.photo.title);
    println!("提示词:{}", payload.photo.prompt);
    println!("动效:{}", payload.photo.motion.summary());
    println!(
        "出图:{}×{} · {} KB",
        payload.photo.width,
        payload.photo.height,
        payload.bytes.len() / 1024
    );
    println!("调色板:{}", payload.photo.palette.join(" "));
    // 顺手落一份到临时目录:命令行里看不了图,但可以直接拿这个文件双击打开
    let preview = std::env::temp_dir().join(format!("mcwp-gen-{}.png", payload.photo.id));
    std::fs::write(&preview, &payload.bytes)
        .with_context(|| format!("写预览图失败:{}", preview.display()))?;
    println!("预览:{}", preview.display());
    println!("{}", payload.photo.to_pretty_json());
    Ok(())
}

/// `--video-selftest <mp4>`:把一段 mp4 解一遍,原样摊开。
/// 抽样均值:和"第 N 帧"那几行用同一个步长,读数能直接对着看。
fn sample_mean(frame: &[u8]) -> f32 {
    if frame.is_empty() {
        return f32::NAN;
    }
    let sampled = frame.len() / 997 + 1;
    let sum: u64 = frame.iter().step_by(997).map(|byte| *byte as u64).sum();
    sum as f32 / sampled as f32
}

/// 首帧 / 末帧 → **旧版**播放到头时该乘的曝光增益(1.0 = 不用校)。
///
/// 它现在只用来给 `Photo.seam_gain` 留一个诊断值,播放端**不再拿它乘整段**
/// (那会把片头暗、后面亮的片子压暗;见 `display_gain_for` / §11.33)。
fn seam_gain_for(first: &[u8], last: &[u8]) -> f32 {
    let (mean_first, mean_last) = (rgb_mean(first), rgb_mean(last));
    if !mean_first.is_finite() || !mean_last.is_finite() || mean_last < 1.0 {
        return 1.0;
    }
    // 夹在 0.75~1.35:真量出一个 2 倍的比值,那说明这不是"曝光在飘",而是解码或画面出了别的事,
    // 硬按它乘只会把画面搞得忽明忽暗。
    let gain = (mean_first / mean_last).clamp(0.75, 1.35);
    if (gain - 1.0).abs() < 0.02 {
        // 差得太小就别校:每帧都要多乘一次,换来的却是看不出的一点点
        1.0
    } else {
        gain
    }
}

/// RGB 三路的均值(不含 alpha)。
fn rgb_mean(frame: &[u8]) -> f32 {
    if frame.is_empty() || frame.len() % 4 != 0 {
        return f32::NAN;
    }
    let mut sum = 0u64;
    let mut count = 0u64;
    for pixel in frame.chunks_exact(4) {
        for channel in 0..3 {
            sum += pixel[channel] as u64;
            count += 1;
        }
    }
    sum as f32 / count.max(1) as f32
}

/// 把 `b` 整体乘一个增益,让它和 `a` 的平均亮度对齐(所谓的 gain match)。
///
/// 为什么不在播放时就做这件事、而先在这里量:`gain match` 只有**曝光差占大头**时才有意义。
/// 要是两帧的差主要是构图跑了,乘一个系数只是让画面变亮变暗,一点都帮不上忙 ——
/// 先量出"对齐之后还剩多少",再决定要不要动播放那边。
fn gain_matched(a: &[u8], b: &[u8]) -> Vec<u8> {
    let (mean_a, mean_b) = (rgb_mean(a), rgb_mean(b));
    if a.len() != b.len() || !mean_a.is_finite() || !mean_b.is_finite() || mean_b < 1.0 {
        return b.to_vec();
    }
    let gain = (mean_a / mean_b).clamp(0.5, 2.0);
    // 一张 256 项的表,一次乘好;逐像素就只剩查表,7 MB 一帧也扛得住
    let lut: Vec<u8> = (0..=255)
        .map(|value| (value as f32 * gain).round().clamp(0.0, 255.0) as u8)
        .collect();
    let mut out = b.to_vec();
    for pixel in out.chunks_exact_mut(4) {
        for channel in 0..3 {
            pixel[channel] = lut[pixel[channel] as usize];
        }
    }
    out
}

/// 两帧的平均绝对差(0 = 一模一样)。
///
/// 只比 RGB 三路:第四路是恒定的 alpha(255),算进去只会把读数按比例稀释。
/// 长度对不上就直接给 `NaN` —— 尺寸中途变了的话,这个数本来就没有意义。
fn mean_abs_diff(a: &[u8], b: &[u8]) -> f32 {
    if a.is_empty() || a.len() != b.len() || a.len() % 4 != 0 {
        return f32::NAN;
    }
    let mut sum = 0u64;
    let mut count = 0u64;
    for (pixel_a, pixel_b) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        for channel in 0..3 {
            sum += pixel_a[channel].abs_diff(pixel_b[channel]) as u64;
            count += 1;
        }
    }
    sum as f32 / count.max(1) as f32
}

/// `--video-selftest <mp4>`:把一段 mp4 解一遍,原样摊开。
///
/// 为什么要它:视频这条路上最容易坏的恰恰是**解码这一段**(MF 的初始化、格式协商、
/// 这台机器上装不装得上那个色彩转换器),而它坏掉的表现只有一句"画面是黑的" ——
/// 那种时候从"库、渲染器、AI 客户端"里找是白费劲。所以这里只解一个文件:
/// 不解库、不建窗口、不碰网络,把"解出来多少帧、每帧多大、像素大概什么样"直接打出来。
///
/// 顺带把**循环点**也量出来(首帧↔末帧 vs 首帧↔中点):"播放完这一下跳不跳"原本只能靠
/// 盯着桌面等十秒,而现在它有数 —— 接不上是模型的锅还是播放的锅,一眼能分开。
fn run_video_selftest(path: &str, screen: (u32, u32)) -> Result<()> {
    let started = Instant::now();
    let mut player = mcwp_video::Player::open(std::path::Path::new(path), screen)?;
    println!("打开用了 {:.0}ms", started.elapsed().as_secs_f32() * 1000.0);
    let info = player.info();
    println!(
        "信息:{}×{} · {:.0}fps · {:.2}s",
        info.width,
        info.height,
        info.fps,
        info.duration.as_secs_f32()
    );
    let begin = Instant::now();
    let mut frames = 0usize;
    let mut frame_bytes = 0usize;
    // 首帧和末帧各留一份:循环点接不接得上,是**比出来的**,不是盯出来的(见文末那几行)。
    let mut first: Vec<u8> = Vec::new();
    let mut last: Vec<u8> = Vec::new();
    while let Some(frame) = player.next_frame()? {
        frames += 1;
        frame_bytes = frame.len();
        if frames == 1 {
            first = frame.to_vec();
        }
        last = frame.to_vec();
        if frames <= 3 {
            // 抽样均值能一眼看出"是不是全黑"。另外第四字节恒为 255:要是均值贴着 255,
            // 说明读到的是 alpha 那一列 —— 那是 stride 用错了。
            let sampled = frame.len() / 997 + 1;
            let sum: u64 = frame.iter().step_by(997).map(|byte| *byte as u64).sum();
            println!(
                "第 {frames} 帧:{} KB · 采样均值 {:.1}",
                frame.len() / 1024,
                sum as f64 / sampled as f64
            );
        }
    }
    let elapsed = begin.elapsed().as_secs_f64().max(0.001);
    println!(
        "共 {frames} 帧 · 每帧 {} KB · 解码 {elapsed:.2}s({:.0} 帧/s)",
        frame_bytes / 1024,
        frames as f64 / elapsed
    );
    // 循环点:把**头尾那两帧差多少**量出来。
    //
    // 为什么要量:循环接缝从外部看只有一个信号 —— 播放完那一下跳不跳。
    // 默认是单图首帧,尾帧本来就不会精确回到起点,播放器靠两端收口。
    // 盯着桌面等十秒去判,既看不准又受当时画面影响;而"末帧离首帧有多远"是个数。
    // 再拿"首帧到中点那帧"当基线,这个数才有尺度:两者相当 = 尾帧没有绕回起点,
    // 接缝完全靠播放器的淡入淡出兜着(单图提交的常态,见 §11.31)。
    let halfway = {
        player.rewind()?;
        let target = frames / 2;
        let mut frame_at = Vec::new();
        for index in 0..target {
            if let Some(frame) = player.next_frame()? {
                if index + 1 == target {
                    frame_at = frame.to_vec();
                }
            }
        }
        frame_at
    };
    let seam = mean_abs_diff(&first, &last);
    let span = mean_abs_diff(&first, &halfway);
    // 把末帧整体乘一个增益、跟首帧的亮度对齐再看一次:**剩下的才是构图上的差**。
    // 这个数决定修法 —— 剩下不多,说明"整体曝光在爬"(播放时按帧校正增益就能压掉);
    // 剩下还很多,说明画面真的跑走了(只能重出片,或者把片子反过来放)。
    let corrected = mean_abs_diff(&first, &gain_matched(&first, &last));
    // 亮度这一栏是为了分清两种"接不上":**整体变亮/变暗**(全局曝光在飘)和
    // **画面真的跑走了**(构图变了)。前者能靠校正压下去,后者只能靠重出片或倒放。
    println!(
        "亮度(采样均值):首帧 {:.1} · 中点 {:.1} · 末帧 {:.1}",
        sample_mean(&first),
        sample_mean(&halfway),
        sample_mean(&last)
    );
    println!("接缝:首帧↔末帧 平均差 {seam:.2}(基线:首帧↔中点那帧 {span:.2})");
    println!("接缝(只把整体亮度对齐后){corrected:.2} —— 剩下的才是构图差");
    if seam.is_nan() || span.is_nan() {
        println!("  → 帧长度对不齐,这个数没意义(尺寸中途变了?)");
    } else if seam <= span * 0.25 {
        println!("  → 头尾基本绕回了起点,0.8 秒的两端收口足以把这点差抹平");
    } else if seam <= span * 0.6 {
        println!("  → 头尾明显比半途近,但没到「绕回」:淡回能盖住跳变,看得出画面在往回并");
    } else {
        println!(
            "  → 头尾和半途一样远:**尾帧没回到起点**(模型只保证局部元素在动,靠播放器两端收口)"
        );
    }
    // 倒回开头再解一帧:循环播放全靠这一步,而它在 MF 上是最容易"静默不生效"的一处
    player.rewind()?;
    println!(
        "倒回开头后再解一帧:{}",
        player
            .next_frame()?
            .map(|frame| format!("{} KB", frame.len() / 1024))
            .unwrap_or_else(|| "没有".to_string())
    );
    Ok(())
}

/// 面板要显示的「当前壁纸」。
///
/// 从 `Photo` 派生出面板要的那几个字段(标题 / 短提示 / 动效摘要 / 调色板 / 身份串),
/// **派生规则只写在这一处**:面板状态、玻璃配色、库网格里那张"当前"高亮都只认它的产物。
///
/// 「桌面上还没有壁纸」是个**合法空态**(第一次启动、系统壁纸又读不出来):那时给一套中性深色,
/// 而不是空数组 —— 面板底色跟着当前壁纸走,空数组会让玻璃掉回默认的亮蓝,和黑桌面配在一起很怪。
struct CurrentWallpaper {
    title: String,
    hint: String,
    summary: String,
    palette: Vec<String>,
    fingerprint: String,
}

/// 没有壁纸时用的中性深色(和早期默认预设的调色板同一族)。
const EMPTY_PALETTE: [&str; 5] = ["#0B1020", "#1B2A4A", "#4F7CFF", "#9BB8FF", "#E6ECFF"];

impl CurrentWallpaper {
    fn of(photo: Option<&Photo>) -> Self {
        let Some(photo) = photo else {
            return Self {
                title: "还没有壁纸".to_string(),
                hint: "在右边的对话里说一句想看到的样子".to_string(),
                summary: String::new(),
                palette: EMPTY_PALETTE.iter().map(|hex| hex.to_string()).collect(),
                fingerprint: String::new(),
            };
        };
        Self {
            title: photo.title.clone(),
            // 提示词就是这张壁纸的"关键词";截成短标题,不然会撑破卡片上那一行
            hint: mcwp_spec::photo::title_from_prompt(&photo.prompt),
            summary: photo_summary(photo),
            palette: photo.palette.clone(),
            // 面板上那串"指纹"用库里的身份:和文件名、库目录里看到的那串字符是同一个
            fingerprint: photo.id.clone(),
        }
    }
}

/// 一条记录在面板上那句摘要。
///
/// 静图写的是动效配方(Ken Burns 推拉多少、速度多少),而视频要说的只有"多长" ——
/// 视频的动效是模型画的,没有参数可讲,拿静图那套去描述只会显示成
/// "静帧呼吸 · 推近 12%"(而它根本不是靠推拉在动)。
fn photo_summary(photo: &Photo) -> String {
    if photo.kind.is_video() {
        return format!("动态壁纸 · {:.1} 秒", photo.duration_ms as f32 / 1000.0);
    }
    photo.motion.summary()
}

/// 把「当前壁纸 + 壁纸库 + 会话」拼成面板的初始状态。
fn panel_state(
    photo: Option<&Photo>,
    sessions: &Sessions,
    library: &[LibraryEntry],
) -> mcwp_ui::PanelState {
    let current = CurrentWallpaper::of(photo);
    let selection = library_selection(photo, library);

    mcwp_ui::PanelState {
        title: current.title,
        hint: current.hint,
        fingerprint: current.fingerprint,
        summary: current.summary,
        palette: current.palette,
        session: sessions.state(),
        library: library_items(library),
        library_current: selection.id,
        library_current_index: selection.index,
    }
}

/// 打开控制面板:把「当前壁纸 + 库 + 会话」拼成初始状态交给 UI 线程。
///
/// `--panel` 启动时和托盘「打开控制面板」都走这里。
fn open_panel(
    photo: Option<&Photo>,
    sessions: &Sessions,
    library: &[LibraryEntry],
    events: &Sender<mcwp_ui::UiEvent>,
) -> Option<mcwp_ui::PanelHandle> {
    let state = panel_state(photo, sessions, library);
    // 面板字体是子集化的,文案里冒出新字就会画不出来,先摊在日志里。
    for (field, missing) in state.missing_chars() {
        tracing::warn!("面板字体缺字:{field} 里的 {missing:?} 不在内嵌子集内");
    }
    match mcwp_ui::open(state, events.clone()) {
        Ok(handle) => {
            tracing::info!("控制面板已打开");
            Some(handle)
        }
        Err(err) => {
            tracing::error!("打开控制面板失败:{err:?}");
            None
        }
    }
}

// —— 图片库搬到 `photos.rs` 了 ——
//
// 老的那一套(一份 spec = 一个 JSON、文件名是 spec 指纹)随"预设清零"一起退休:
// 它服务的世界是"画面由本地着色器现算",那个世界正在被生图取代。
// 新的库是"一张图 + 一份元数据",去重也不再是卖点 —— 同一句提示词两次生成本来就是两张图。

/// 壁纸库里的一条:**面板要显示的那份** + **它在盘上的真身**。
///
/// 两样都留着是因为两件事都要用它:点卡片要按 `source.image` 现解一次图,右键删除要按
/// `source` 自己记着的那三个路径删(`photos::remove` 只认那一条记录,不按 id 通配)。
struct LibraryEntry {
    view: mcwp_ui::LibraryEntryView,
    source: photos::Entry,
}

/// 盘上的一条记录 → 库里的一条。
///
/// 缩略图**在这里就解好**:面板一屏要几十张,每张现解一次 1080p 原图就是好几秒起步,
/// 而缩略图是几百字节的 PNG,读它几乎不花时间(`photos::Entry::read_thumb` 缺图时还会
/// 顺手补一张存下去)。解出来的裸像素交给面板线程去装 `slint::Image` —— 见 `LibraryEntryView`。
fn library_entry(source: photos::Entry) -> LibraryEntry {
    let photo = &source.photo;
    let view = mcwp_ui::LibraryEntryView {
        // 照片的 id 就是它的文件名,天然唯一,也天然和"当前壁纸"比对得上
        id: photo.id.clone(),
        title: photo.title.clone(),
        // 提示词限长:`hint` 是卡片上那行小字,长提示词会把卡片撑到两行
        hint: mcwp_spec::photo::title_from_prompt(&photo.prompt),
        summary: photo_summary(photo),
        video: source.is_video(),
        // 出处:面板靠它知道"会话里那张静图已经做成动图了"(见 `LibraryEntryView::origin_id`)
        origin_id: photo.origin_id.clone(),
        // 卡片的主色用照片自己提出来的强调色 —— 面板玻璃也跟着它染色,
        // 所以切壁纸的时候整个面板的色调会一起变,而不是只有壁纸变了
        accent: mcwp_ui::glass_colors(&photo.palette).accent(),
        thumb: thumb_pixels(&source),
    };
    LibraryEntry { view, source }
}

/// 一条库记录的缩略图,裸像素(RGBA8 + 宽高)。
///
/// 读不出来**不致命**:那张原图可能坏了,但标题还在 —— 给 1×1 全透明,格子里就只剩
/// 底色和标题,好过整张卡片消失(用户还能右键把它删掉)。这里只记一条日志不往外抛,
/// 是因为调用方在扫库的循环里,一坏就中断会让**整个库**都打不开。
fn thumb_pixels(entry: &photos::Entry) -> (u32, u32, Vec<u8>) {
    match entry.read_thumb() {
        Ok(bitmap) => (bitmap.width, bitmap.height, bitmap.rgba),
        Err(err) => {
            tracing::warn!("缩略图读不出来({}):{err}", entry.thumb.display());
            (1, 1, vec![0, 0, 0, 0])
        }
    }
}

/// 扫一遍壁纸库,最近生成的排最前。
///
/// 面板不碰文件系统:库列表的真相在壁纸这一侧,面板只是它的投影 —— 和会话数据一个道理。
/// 库目录是用户可写的地方(手工删文件、拷别人的壁纸进来都算正常用法),坏条目只记一条日志跳过。
fn scan_library() -> Vec<LibraryEntry> {
    photos::scan().into_iter().map(library_entry).collect()
}

/// 交给面板的那份库列表(只有显示数据,照片真身留在这一侧)。
fn library_items(library: &[LibraryEntry]) -> Vec<mcwp_ui::LibraryEntryView> {
    library.iter().map(|entry| entry.view.clone()).collect()
}

/// 当前照片在库里的下标。不在库里(刚生成、扫库还没跟上)就当"最后一张" ——
/// 这样托盘"换一张"的 `+1` 会绕回第一张,而不是从第一张跳到第二张。
fn current_photo_index(active: &Option<ActivePhoto>, library: &[LibraryEntry]) -> usize {
    let Some(photo) = active.as_ref().map(|it| &it.photo) else {
        return 0;
    };
    library
        .iter()
        .position(|entry| entry.source.photo.id == photo.id)
        .unwrap_or_else(|| library.len().saturating_sub(1))
}

/// 面板画库页时要的那一小撮"当前是哪张":id(高亮用)、下标(那颗「删除」用)、标题(确认框用)。
///
/// 三样必须**一起**算出来。分开算的话,迟早出现"高亮的是这张、删掉的却是那张" ——
/// 而它们的来源是同一个 `active` 和同一份 `library`,本来就没有分开的理由。
struct LibrarySelection {
    id: String,
    index: i32,
}

/// 当前生效的壁纸在库里的位置。**不在库里**时 id 是空串、下标是 -1(比如刚抬进来、扫库还没跟上):
/// 那种情况下既不该高亮某一格,也没有"这张"可删。
fn library_selection(photo: Option<&Photo>, library: &[LibraryEntry]) -> LibrarySelection {
    let Some(photo) = photo else {
        return LibrarySelection {
            id: String::new(),
            index: -1,
        };
    };
    match library
        .iter()
        .position(|entry| entry.source.photo.id == photo.id)
    {
        Some(index) => LibrarySelection {
            id: photo.id.clone(),
            index: index as i32,
        },
        None => LibrarySelection {
            id: String::new(),
            index: -1,
        },
    }
}

#[derive(Debug)]
struct Args {
    seconds: Option<f32>,
    fps: u32,
    brightness: f32,
    standalone_window: bool,
    attach_mode: AttachMode,
    show_host: bool,
    dump: bool,
    /// 只建窗口、不建渲染器,用 GDI 背景刷填充(排查合成问题)
    gdi_only: bool,
    /// 用普通窗口样式(排查 DXGI 呈现问题)
    plain_window: bool,
    /// 离屏渲染一次并回读中心像素,用来区分渲染问题与呈现问题
    selftest: bool,
    /// 启动时打开控制面板
    panel: bool,
    /// 开面板,并替用户把这句话发出去
    ai_ask: Option<String>,
    /// 现场生一张图并换上(整条链路:提示词智能体 → 生图 → 上屏)
    generate: Option<String>,
    /// 换上壁纸库里已有的一张(id = 库目录里的文件名)
    photo_id: Option<String>,
    /// 把桌面上现在那张壁纸**再**抬一次(库空时本来就会自动抬,这个参数是给
    /// 「换过桌面壁纸之后想把它收进来」准备的)
    adopt: bool,
    /// 只跑一遍生图链路、打印结果就退出(不建窗口)
    images_selftest: Option<String>,
    /// 只跑**提示词那一步**、把请求和模型给的计划打印出来就退出(不建窗口、不出图)
    plan: Option<String>,
    /// `--plan` 的伴生参数:带上"现在屏幕上那张"(库 id),复现面板里的上下文
    plan_current: Option<String>,
    /// `--motion-plan <库 id>`:只跑**看图写动效**那一步,把元素级方案和最终那句提示词打印出来
    motion_plan: Option<String>,
    /// `--animate-save <库 id>`:跑完整条「做成动态壁纸」链路(**要花钱那一步**),
    /// 产物进壁纸库,但**不上屏、不建窗口**
    animate_save: Option<String>,
    /// `--video-fetch <任务号> <库 id>`:照着任务号把**已经提交过**的那条视频取回来入库。
    /// 不提交、不再花钱 —— 专门给"等超时丢掉的那 1 元"准备的回收口
    video_fetch: Option<(String, String)>,
    /// 把库里某一张(静图)**做成动态壁纸**,跑完整条链路再上屏(不建面板)
    animate: Option<String>,
    /// 只解一段 mp4、打印几帧的统计就退出(不建窗口)—— 视频解码那条路唯一的自检入口。
    /// 它解的是**任意 mp4 文件**,不是库里的条目:解码器坏了和库坏了是两件事,分开测。
    video_selftest: Option<String>,
    stats_interval: Duration,
}

impl Args {
    fn parse() -> Self {
        let mut args = Self {
            seconds: None,
            fps: 60,
            brightness: 1.0,
            standalone_window: false,
            attach_mode: AttachMode::Auto,
            show_host: false,
            dump: false,
            gdi_only: false,
            plain_window: false,
            selftest: false,
            panel: false,
            ai_ask: None,
            generate: None,
            photo_id: None,
            adopt: false,
            images_selftest: None,
            plan: None,
            plan_current: None,
            motion_plan: None,
            animate_save: None,
            video_fetch: None,
            animate: None,
            video_selftest: None,
            stats_interval: Duration::from_secs(5),
        };
        let raw: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < raw.len() {
            let key = raw[i].as_str();
            let value = raw.get(i + 1);
            match key {
                "--seconds" => {
                    args.seconds = value.and_then(|v| v.parse().ok());
                    i += 2;
                }
                "--fps" => {
                    if let Some(v) = value.and_then(|v| v.parse().ok()) {
                        args.fps = v;
                    }
                    i += 2;
                }
                "--brightness" => {
                    if let Some(v) = value.and_then(|v| v.parse().ok()) {
                        args.brightness = v;
                    }
                    i += 2;
                }
                "--stats" => {
                    if let Some(v) = value.and_then(|v| v.parse().ok()) {
                        args.stats_interval = Duration::from_secs(v);
                    }
                    i += 2;
                }
                "--window" => {
                    args.standalone_window = true;
                    i += 1;
                }
                "--show-host" => {
                    args.show_host = true;
                    i += 1;
                }
                "--dump" => {
                    args.dump = true;
                    i += 1;
                }
                "--gdi-only" => {
                    args.gdi_only = true;
                    i += 1;
                }
                "--plain" => {
                    args.plain_window = true;
                    args.standalone_window = true;
                    i += 1;
                }
                "--selftest" => {
                    args.selftest = true;
                    i += 1;
                }
                "--panel" => {
                    args.panel = true;
                    i += 1;
                }
                "--ai-ask" => {
                    args.ai_ask = value.cloned();
                    i += 2;
                }
                "--gen" => {
                    args.generate = value.cloned();
                    i += 2;
                }
                "--photo" => {
                    args.photo_id = value.cloned();
                    i += 2;
                }
                "--adopt" => {
                    args.adopt = true;
                    i += 1;
                }
                "--images-selftest" => {
                    args.images_selftest = value.cloned();
                    i += 2;
                }
                "--plan" => {
                    args.plan = value.cloned();
                    i += 2;
                }
                "--plan-current" => {
                    args.plan_current = value.cloned();
                    i += 2;
                }
                "--animate" => {
                    args.animate = value.cloned();
                    i += 2;
                }
                "--plan-motion" => {
                    args.motion_plan = value.cloned();
                    i += 2;
                }
                "--animate-save" => {
                    args.animate_save = value.cloned();
                    i += 2;
                }
                "--video-fetch" => {
                    // 两个值:任务号在前(要"取回"的就是它),库 id 在后(要挂到哪张图上)
                    let task = value.cloned().unwrap_or_default();
                    let photo = raw.get(i + 2).cloned().unwrap_or_default();
                    args.video_fetch = Some((task, photo));
                    i += 3;
                }
                "--video-selftest" => {
                    args.video_selftest = value.cloned();
                    i += 2;
                }
                "--attach" => {
                    match value.and_then(|v| AttachMode::parse(v)) {
                        Some(mode) => args.attach_mode = mode,
                        None => eprintln!("--attach 只支持 {}", AttachMode::NAMES),
                    }
                    i += 2;
                }
                other => {
                    eprintln!("忽略未知参数:{other}");
                    i += 1;
                }
            }
        }
        args
    }
}

fn main() -> Result<()> {
    let (ws0, private0) = memory_mb();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let args = Args::parse();
    tracing::info!("启动内存:工作集={ws0:.1}MB 私有={private0:.1}MB");

    // 排查用的那几个参数要么不挂桌面要么只是看状态,不该被单实例挡住。
    let is_diagnostic_run = args.standalone_window
        || args.plain_window
        || args.dump
        || args.gdi_only
        || args.selftest
        || args.images_selftest.is_some()
        || args.plan.is_some()
        || args.motion_plan.is_some()
        || args.animate_save.is_some()
        || args.video_fetch.is_some()
        || args.video_selftest.is_some();
    if !is_diagnostic_run && !claim_single_instance() {
        return Ok(());
    }

    // 「只跑一遍生图」同理:不建窗口、不占桌面,接生图时唯一需要的调试入口。
    if let Some(request) = args.images_selftest.as_deref() {
        // 屏幕尺寸在这里是**写死的 1920×1080**(窗口还没建,读屏幕也读不到 DPI 感知后的真值)。
        // 自从尺寸改成"屏幕多大就要多大"(§11.27),这条路验的东西也就变了:它验的是
        // "折出来的尺寸对不对 + 服务商收不收",**不再**是"你这台机器的屏到底是多大"。
        // 要看真机那一份,只能在面板里生一张,然后读那行 `生图完成:W×H`。
        run_images_selftest(request, (1920, 1080))?;
        return Ok(());
    }
    // 「只看提示词那一步」:一次调用几秒钟,调提示词时不用等图、不用建窗口、不进库。
    if let Some(request) = args.plan.as_deref() {
        run_plan_selftest(request, args.plan_current.as_deref())?;
        return Ok(());
    }
    // 「只看动效分析」:看图一次、免费,把元素级方案和最终那句提示词摊开 ——
    // 出片那一步要花钱(1 元/次),所以**调提示词必须能只跑前半段**。
    if let Some(id) = args.motion_plan.as_deref() {
        run_motion_selftest(id)?;
        return Ok(());
    }
    // 「出片但不上屏」:这一步真要花钱(1 元/次)也要等几分钟,所以它必须能**在后台验证** ——
    // 不建窗口(也就不会跟正在跑的面板抢单实例那把锁),更不会把用户桌面换掉。
    if let Some(id) = args.animate_save.as_deref() {
        // 屏幕尺寸在这里只用来定"封面解到多大",还不知道真实屏幕,按 1080p 给
        run_animate_save(id, (1920, 1080))?;
        return Ok(());
    }
    // 「照任务号取回」:超时丢掉的那条任务**服务端还在跑、钱已经花了**,重提就是再花一次。
    // 它同样不建窗口,只是把一个已经算好的结果下载下来入库。
    if let Some((task, photo)) = args.video_fetch.as_ref() {
        run_video_fetch(task, photo, (1920, 1080))?;
        return Ok(());
    }
    // 「只解一段视频」同理:不建窗口、不占桌面,接解码那条路时唯一需要的调试入口。
    if let Some(path) = args.video_selftest.as_deref() {
        // 屏幕尺寸在这里只用来定"解出来缩到多大" —— 还不知道真实屏幕,按 1080p 给
        run_video_selftest(path, (1920, 1080))?;
        return Ok(());
    }
    #[cfg(not(feature = "dev-tools"))]
    if args.dump || args.gdi_only || args.plain_window || args.selftest || args.show_host {
        tracing::warn!("检测到排查用参数,但本次构建未启用 `dev-tools` 特性,已忽略");
    }

    if set_per_monitor_dpi_awareness() {
        tracing::info!("已启用 Per-Monitor V2 DPI 感知");
    } else {
        tracing::warn!("设置 DPI 感知失败,多显示器/高缩放下窗口位置可能不正确");
    }

    tracing::info!("启动参数:{args:?}");

    let rect = primary_monitor_rect();
    let (_, _, width, height) = rect;
    tracing::info!("主显示器:{width}x{height} @ ({}, {})", rect.0, rect.1);

    #[cfg(feature = "dev-tools")]
    if args.dump {
        println!("{}", candidate_report());
        diag::dump_tree(2);
    }

    let style = if cfg!(feature = "dev-tools") && args.plain_window {
        HostStyle::Plain
    } else {
        HostStyle::Wallpaper
    };
    let window = HostWindow::create_with(rect, style)?;
    let (ws1, private1) = memory_mb();
    tracing::info!("建窗后内存:工作集={ws1:.1}MB 私有={private1:.1}MB");
    if args.standalone_window {
        trace_chain(window.hwnd(), "挂载前(--window)");
        window.show_standalone()?;
    } else {
        let show_host = cfg!(feature = "dev-tools") && args.show_host;
        let desc = window.attach_to_desktop(args.attach_mode, show_host)?;
        trace_chain(window.hwnd(), &format!("挂载后({desc})"));
    }

    // GDI 自检:不建渲染器,让窗口用类的背景刷填充,用来判断窗口是否被合成。
    #[cfg(feature = "dev-tools")]
    if args.gdi_only {
        set_gdi_background_paint(true);
        trace_chain(window.hwnd(), "GDI 测试窗口");
        invalidate_window(window.hwnd());
        let deadline = Instant::now() + Duration::from_secs_f32(args.seconds.unwrap_or(8.0));
        window.run_until(deadline);
        tracing::info!("GDI 自检结束");
        return Ok(());
    }

    let mut renderer = Renderer::new(window.hwnd().0 as isize, width as u32, height as u32)?;
    let (ws2, private2) = memory_mb();
    tracing::info!("建渲染器后内存:工作集={ws2:.1}MB 私有={private2:.1}MB");

    let brightness = args.brightness.clamp(0.2, 1.6);
    // 这一次运行绑定的屏幕尺寸。照片的 cover 裁剪、生图的请求尺寸都用它。
    let screen = (width as u32, height as u32);
    // 当前挂在屏幕上的照片;`None` = 这一次还没有可画的东西(库空、桌面上也抬不到图)。
    // 那种情况下屏幕上是 `Renderer::new` 铺的那块占位色 —— 认得出是"还没画",不是黑屏。
    let mut active: Option<ActivePhoto> = None;

    // —— 模型 ——
    // 没配就只是"少个能力",不是错误:面板照常能开、能聊,回复位会说清楚去哪儿配。
    // 两个都是 `mut`:设置页保存之后要就地换掉(见 `save_ai_config`)。
    let mut ai: Option<mcwp_ai::Client> = None;
    // 生图是**独立的一套配置**:同一把 key 默认共用,也可以指向另一家(见 `ImagesConfig`)。
    // 它和对话客户端分开建,是因为超时差一个数量级(45 s vs 180 s)、地址也不同。
    let mut images: Option<mcwp_ai::Images> = None;
    // 图生视频同理:地址和 key 跟着生图那组走,单独建客户端是因为它是**两段式**的
    // (提交 → 轮询),而且一条要跑一分钟(见 `Videos` 的说明)。
    let mut videos: Option<mcwp_ai::Videos> = None;
    rebuild_clients(&mut ai, &mut images, &mut videos);
    // 请求跑在后台线程,结果从这里回到主循环
    let (ai_tx, ai_rx) = std::sync::mpsc::channel::<AiOutcome>();
    // 「做成动态壁纸」走**自己的一条通道**:它不属于任何会话(是按钮触发的,不是聊天),
    // 也不参与"暂停 / 继续"那一套,混进 `AiOutcome` 只会让那边的轮次判断多一堆分支。
    let (video_tx, video_rx) = std::sync::mpsc::channel::<VideoOutcome>();
    // 当前这一轮(在跑 / 被暂停 / 没有)。`run_id` 只增不减:每起一轮换一个,结果回来时拿它对
    // 暗号 —— 暂停之后、或者又发了新的一句之后,旧请求的结果会晚一步回来,对不上就丢掉。
    let mut run: Option<Run> = None;
    let mut run_id: u64 = 0;
    // 正在做的那一段视频的轮次(没在做就是 `None`)。同一时刻只允许跑一轮:
    // 它要一分钟、还要花钱,点两下就并排跑两条是纯粹的浪费。
    let mut video_run: Option<u64> = None;
    let mut video_id: u64 = 0;
    // 正在做动图的那张图的 id。面板靠它把对应那一条回复切成"正在生成动图…" ——
    // 空 = 没有在做的。**只有真起跑了才写它**(见 `start_video_run_visible`)。
    let mut video_target: Option<String> = None;

    // —— 控制面板 ——
    // 面板跑在自己的线程和自己的 Slint 事件循环里;这里只保留一个事件队列,
    // 每帧开头把它清空一次,所以拖动滑杆的下一帧就能看到变化。
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<mcwp_ui::UiEvent>();
    // 会话数据在壁纸这一侧(Rust),面板只是它的投影;面板关掉再开(甚至整个进程重启),
    // 历史都在 —— 落盘那份就在 `%APPDATA%\MC壁纸\sessions.json`,和壁纸库同目录。
    let mut sessions = Sessions::load();
    // 壁纸库同理:开面板时先扫一遍库目录,面板打开就是最新的一张列表。
    let mut library = scan_library();
    tracing::info!("壁纸库里有 {} 张壁纸", library.len());

    // —— 起手那张壁纸 ——
    // 优先级:`--gen`(现场生成)> `--animate`(把库里那张做成动态壁纸)> `--photo`(点库里的一张)
    // > 抬桌面上那张 > 库里最新的一张。
    if let Some(request) = args.generate.clone() {
        match (ai.as_ref(), images.as_ref()) {
            (Some(chat), Some(painter)) => {
                tracing::info!("`--gen`:先画一张「{request}」");
                let started = Instant::now();
                let payload = generate_photo(chat, painter, &request, None, screen)
                    .context("`--gen` 生图失败")?;
                tracing::info!("生图耗时 {:.1}s", started.elapsed().as_secs_f32());
                active = Some(adopt_photo(
                    &mut renderer,
                    &mut library,
                    payload,
                    screen,
                    brightness,
                )?);
            }
            _ => tracing::warn!("`--gen` 需要先配好模型与生图 key,这次跳过"),
        }
    } else if let Some(id) = args.animate.clone() {
        match photos::load(&id) {
            Ok(entry) => match videos.as_ref() {
                Some(animator) => {
                    tracing::info!(
                        "`--animate`:把「{}」做成动态壁纸(要几分钟)",
                        entry.photo.title
                    );
                    let payload = animate_photo(ai.as_ref(), animator, &entry, screen)
                        .context("`--animate` 失败")?;
                    active = Some(adopt_video(
                        &mut renderer,
                        &mut library,
                        payload,
                        screen,
                        brightness,
                    )?);
                }
                None => tracing::warn!("`--animate` 需要先配好图生视频的 key(和生图同一把)"),
            },
            Err(err) => tracing::warn!("`--animate {id}` 在库里找不到那张图:{err:?}"),
        }
    } else if let Some(id) = args.photo_id.clone() {
        match photos::load(&id).and_then(|entry| match entry.video.clone() {
            // 视频条目:放的是**那段视频**,不是它那张封面静图
            Some(path) => show_video(&mut renderer, entry.photo, &path, screen, brightness),
            None => {
                let bitmap = entry.read_image()?;
                show_photo(&mut renderer, entry.photo, &bitmap, screen, brightness)
            }
        }) {
            Ok(photo) => {
                tracing::info!("`--photo`:已换上 {}", photo.photo.title);
                active = Some(photo);
            }
            Err(err) => tracing::warn!("`--photo {id}` 没换成功:{err:?}"),
        }
    } else {
        // 抬桌面上那张。两种情况下做:`--adopt` 是用户显式点的(换过桌面壁纸之后想收进来),
        // 库里空着时则**自动做** —— 见 `adopt_desktop` 的说明,这是"第一次启动就有东西看"的兜底。
        if args.adopt || library.is_empty() {
            match adopt_desktop(&mut renderer, &mut library, screen, brightness) {
                Ok(photo) => {
                    tracing::info!("已把桌面上那张抬进来:{}", photo.photo.title);
                    active = Some(photo);
                }
                Err(err) => {
                    if args.adopt {
                        tracing::warn!("`--adopt` 没抬成:{err:?}");
                    } else {
                        tracing::info!("壁纸库是空的,桌面上也没有可用的图:{err:?}");
                    }
                }
            }
        }
        // 还没定下画什么,就回落到库里最新的一张(上次退出时挂着的大概率就是它)。
        // 抬桌面壁纸已经上过屏的情况**不走这里** —— 那等于把同一张图再从盘上解一遍。
        if active.is_none() {
            if let Some(first) = library.first() {
                let title = first.view.title.clone();
                match switch_to_photo(&mut renderer, &library, 0, screen, brightness) {
                    Ok(photo) => {
                        tracing::info!("沿用库里最新的一张:{title}");
                        active = Some(photo);
                    }
                    Err(err) => tracing::warn!("沿用上次的壁纸失败,先留着占位色:{err:?}"),
                }
            }
        }
    }

    // 自检:把当前这一刻的参数渲染到离屏纹理再回读中心像素。洋红 = 绘制没发生(管线问题),
    // 回读到画面本身 = 渲染是好的(那问题就在呈现 / 合成环节)。没有可画的东西就没得回读。
    #[cfg(feature = "dev-tools")]
    if args.selftest {
        match &mut active {
            Some(photo) => {
                photo.params.time = 1.0;
                let pixel = renderer.readback_center(&photo.params)?;
                println!("离屏回读中心像素 = {pixel:?}(洋红=(255,0,255) 表示绘制没发生)");
            }
            None => println!("没有壁纸可回读(库是空的,桌面上也没抬到图)"),
        }
        return Ok(());
    }

    let mut panel = if args.panel {
        open_panel(
            active.as_ref().map(|it| &it.photo),
            &sessions,
            &library,
            &ui_tx,
        )
    } else {
        None
    };

    // 设置页的模型配置:开面板时灌一次。之后只在「保存成功」时再灌 —— 平时不碰它,免得把
    // 用户正在输入的 key 冲掉(理由见 `mcwp_ui::AiConfigState` 的说明)。
    if let Some(handle) = &panel {
        handle.refresh_ai(ai_config_state());
        // 面板可能是"跑到一半才打开"的(面板能关掉再开),所以这一份也得灌 —— 不然那颗钮会
        // 停在「发送」上,而底下其实正跑着一轮。
        handle.set_run_state(run_state(run.as_ref()));
    }

    // `--ai-ask "…"`:开面板之后顺手把这句话发出去。命令行也能生成壁纸,
    // 顺带成了唯一的"整条链路"自检方式 —— 合成的键盘输入会被本机中文输入法吃掉,打字那条路验不了。
    if let Some(text) = args.ai_ask.clone() {
        run_id += 1;
        let current = active.as_ref().map(|it| it.photo.clone());
        let (status, mission) = start_run(
            &text,
            &mut sessions,
            &ai,
            &images,
            current,
            screen,
            &ai_tx,
            run_id,
        );
        run = mission.map(|mission| Run::Running {
            id: run_id,
            mission,
        });
        if let Some(handle) = &panel {
            handle.refresh_session(sessions.state());
            handle.set_run_state(run_state(run.as_ref()));
            handle.set_status(status);
        }
    }

    // —— 托盘 ——
    // 和宿主窗口同一个线程:宿主主循环的 `PeekMessageW` 取的是整条线程的消息队列,
    // 托盘窗口的回调会被它顺带派发。所以这里既不需要额外线程,也不影响下面 60fps 的节奏。
    let _tray = match Tray::install(mcwp_ui::TRAY_ICON_ICO, "MC 壁纸 · 正在运行") {
        Ok(tray) => {
            tracing::info!("托盘图标已挂上");
            Some(tray)
        }
        Err(err) => {
            tracing::error!("挂托盘图标失败:{err:?}");
            None
        }
    };

    let started = Instant::now();
    let frame_budget = Duration::from_secs_f64(1.0 / args.fps.max(1) as f64);
    let mut frames_in_window = 0u32;
    let mut rendered_total = 0u64;
    let mut last_stats = Instant::now();
    let mut paused = false;
    // 动画时钟按帧累加,而不是直接取墙钟 —— 暂停几分钟再继续,动画从停下的地方接着走。
    let mut clock = 0.0f32;
    let mut last_tick = Instant::now();

    while window.pump_pending() {
        // 这一帧要切到库里哪一张:面板点卡片、托盘"换一张"都往这里投票,循环末尾统一落地。
        let mut wanted_photo: Option<usize> = None;
        while let Ok(event) = ui_rx.try_recv() {
            tracing::debug!("面板事件:{event:?}");
            match event {
                // —— 生图界面:一句话 → 模型给一份壁纸规格 → 换上去 ——
                mcwp_ui::UiEvent::SendMessage(text) => {
                    if text.trim().is_empty() {
                        continue;
                    }
                    // 用户敲的字面大概率超出内嵌字体子集(生僻字、emoji),缺字是静默不画,先记一笔。
                    let missing = mcwp_ui::missing_chars(&text);
                    if !missing.is_empty() {
                        tracing::warn!(
                            "这条消息里有 {} 个字符不在内嵌字体内:{missing:?}",
                            missing.len()
                        );
                    }
                    // 记进会话 + 丢给后台线程;结果几秒后从 ai_rx 回来。
                    //
                    // 正在跑的时候用户又发一句(包括"暂停之后打字再发"):上一轮就地作废 ——
                    // 换一个 run_id 就够,旧结果回来认不上轮次,自己会被丢掉。用户的原话是
                    // "发送之后跑下一个",所以这里不是排队,是**取代**。
                    run_id += 1;
                    let current = active.as_ref().map(|it| it.photo.clone());
                    let (status, mission) = start_run(
                        &text,
                        &mut sessions,
                        &ai,
                        &images,
                        current,
                        screen,
                        &ai_tx,
                        run_id,
                    );
                    run = mission.map(|mission| Run::Running {
                        id: run_id,
                        mission,
                    });
                    if let Some(handle) = &panel {
                        handle.refresh_session(sessions.state());
                        handle.set_run_state(run_state(run.as_ref()));
                        handle.set_status(status);
                    }
                }
                // 暂停:把这一轮的请求作废,但**任务本身留着** —— 用户点「继续」要原地重跑它。
                //
                // 说实话:HTTP 请求掐不断(ureq 没有取消接口),所以这里是"回来就丢掉"——
                // 线程照样跑完(几秒),结果带着旧的 run_id 回主循环,认不上就直接扔。
                // 不这么做的话只能等它跑完才能响应,那"暂停"就成了假的。
                mcwp_ui::UiEvent::PauseRun => {
                    if let Some(Run::Running { mission, .. }) = run.take() {
                        tracing::info!("暂停这一轮(会话 {},那句话留着待续)", mission.session);
                        run = Some(Run::Paused(mission));
                        if let Some(handle) = &panel {
                            handle.set_run_state(mcwp_ui::RunState::Paused);
                            handle.set_status("已暂停 · 点继续接着跑这一句");
                        }
                    }
                }
                // 继续:把暂停那一轮原样再跑一遍。
                //
                // **不重新往会话里塞用户那句话** —— 它已经在会话里了,再塞一遍就变成两条。
                // 真正重发的是请求本身(模型那边不知道我们暂停过)。
                mcwp_ui::UiEvent::ResumeRun => {
                    if let Some(Run::Paused(mission)) = run.take() {
                        // 会话可能在暂停期间被删了,那就别再续了
                        if !sessions.exists(mission.session) {
                            tracing::info!("暂停那一轮的会话已经被删掉,不再继续");
                            if let Some(handle) = &panel {
                                handle.set_run_state(mcwp_ui::RunState::Idle);
                                handle.set_status("这条会话已经删掉了");
                            }
                            continue;
                        }
                        run_id += 1;
                        let current = active.as_ref().map(|it| it.photo.clone());
                        let (status, mission) = run_mission(
                            mission,
                            &mut sessions,
                            &ai,
                            &images,
                            current,
                            screen,
                            &ai_tx,
                            run_id,
                        );
                        run = mission.map(|mission| Run::Running {
                            id: run_id,
                            mission,
                        });
                        if let Some(handle) = &panel {
                            handle.set_run_state(run_state(run.as_ref()));
                            handle.set_status(status);
                        }
                    }
                }
                mcwp_ui::UiEvent::SelectConversation(index) => {
                    sessions.select(index);
                    if let Some(handle) = &panel {
                        handle.refresh_session(sessions.state());
                    }
                }
                mcwp_ui::UiEvent::NewConversation => {
                    sessions.create();
                    if let Some(handle) = &panel {
                        handle.refresh_session(sessions.state());
                        handle.set_status("新对话");
                    }
                }
                mcwp_ui::UiEvent::RenameConversation(index, name) => {
                    sessions.rename(index, &name);
                    if let Some(handle) = &panel {
                        handle.refresh_session(sessions.state());
                        handle.set_status(format!("已重命名为「{}」", name.trim()));
                    }
                }
                mcwp_ui::UiEvent::DeleteConversation(index) => {
                    sessions.remove(index);
                    if let Some(handle) = &panel {
                        handle.refresh_session(sessions.state());
                        handle.set_status("已删除会话");
                    }
                }
                mcwp_ui::UiEvent::TogglePinConversation(index) => {
                    let pinned = sessions.toggle_pin(index);
                    if let Some(handle) = &panel {
                        handle.refresh_session(sessions.state());
                        handle.set_status(if pinned {
                            "已置顶"
                        } else {
                            "已取消置顶"
                        });
                    }
                }
                mcwp_ui::UiEvent::MarkConversationUnread(index) => {
                    sessions.mark_unread(index);
                    if let Some(handle) = &panel {
                        handle.refresh_session(sessions.state());
                        handle.set_status("已标为未读");
                    }
                }
                // 库卡片右键「删除这一张」:**只动库,不动屏**。
                //
                // 正在看的那张被删掉时画面不会花 —— 纹理已经在显存里了,盘上的文件没了也不影响
                // 这一帧和后面每一帧。但那条记录从"库"里摘掉了,所以面板上的高亮会跟着掉
                // (`library_current` 现算),下次重启也不会再挂着它。
                mcwp_ui::UiEvent::DeleteWallpaper(index) => {
                    let Some(entry) = library.get(index) else {
                        continue;
                    };
                    let title = entry.view.title.clone();
                    match photos::remove(&entry.source) {
                        Ok(()) => {
                            tracing::info!("已从壁纸库删除「{title}」");
                            library = scan_library();
                            if let Some(handle) = &panel {
                                // 整块回填(不是只刷库页):删掉的那张可能正是某条历史消息的
                                // 配图,消息那边也要跟着变成"这张已经不在壁纸库里了"。
                                handle.refresh(panel_state(
                                    active.as_ref().map(|it| &it.photo),
                                    &sessions,
                                    &library,
                                ));
                                handle.set_status(format!("已删除「{title}」"));
                            }
                        }
                        Err(err) => {
                            tracing::warn!("删除「{title}」失败:{err:?}");
                            if let Some(handle) = &panel {
                                let reason = brief(&format!("{err:#}"));
                                handle.set_status(format!("删不掉:{reason}"));
                            }
                        }
                    }
                }
                // 库页工具栏「导入图片」:开系统的选文件框,把选中的图收进库里。
                //
                // 选文件这一步会**阻塞主循环**(`GetOpenFileNameW` 是个模态对话框,自己跑消息
                // 循环),所以这几秒桌面壁纸是停在最后一帧的。可以接受:用户此刻正在挑文件。
                mcwp_ui::UiEvent::ImportWallpaper => {
                    match mcwp_platform::file_dialog::pick_image() {
                        // 取消了。什么都不做,也不要报"失败" —— 用户自己按的取消。
                        None => tracing::info!("导入:用户取消了选文件"),
                        Some(path) => {
                            match import_photo(
                                &mut renderer,
                                &mut library,
                                &path,
                                screen,
                                brightness,
                            ) {
                                Ok(photo) => {
                                    let title = photo.photo.title.clone();
                                    tracing::info!("已导入并换上:{title}(来自 {})", path.display());
                                    active = Some(photo);
                                    if let Some(handle) = &panel {
                                        handle.refresh(panel_state(
                                            active.as_ref().map(|it| &it.photo),
                                            &sessions,
                                            &library,
                                        ));
                                        // 导入**顺手就换上**了(和"点一张就换上去"同一套动作),
                                        // 状态栏就把这件事说全:只写"已导入"会让人以为桌面没变。
                                        handle.set_status(format!("已导入并换上「{title}」"));
                                    }
                                }
                                Err(err) => {
                                    tracing::warn!("导入 {} 失败:{err:?}", path.display());
                                    if let Some(handle) = &panel {
                                        let reason = brief(&format!("{err:#}"));
                                        handle.set_status(format!("导入失败:{reason}"));
                                    }
                                }
                            }
                        }
                    }
                }
                // 点了消息里那张预览图:把大图打开。
                //
                // 解码放主线程(和切壁纸那条路同一个态度):一张 1080p 的图约 200~400 ms,
                // 换来的是"不用为一次点击引入一路后台任务 + 结果通道"。尺寸封顶 1800,
                // 4K 原图进来看大图也一样清晰,但省掉一半像素的拷贝。
                mcwp_ui::UiEvent::OpenPhoto(id) => match photos::load(&id) {
                    Ok(entry) => {
                        let title = entry.photo.title.clone();
                        match entry.read_image() {
                            Ok(bitmap) => {
                                let big = bitmap.thumbnail(1800, 1800);
                                if let Some(handle) = &panel {
                                    handle.show_viewer(title, (big.width, big.height, big.rgba));
                                }
                            }
                            Err(err) => {
                                tracing::warn!("打开「{title}」的大图失败:{err:?}");
                                if let Some(handle) = &panel {
                                    let reason = brief(&format!("{err:#}"));
                                    handle.set_status(format!("这张打不开:{reason}"));
                                }
                            }
                        }
                    }
                    Err(err) => {
                        tracing::warn!("看大图:库里找不到 {id}:{err:?}");
                        if let Some(handle) = &panel {
                            handle.set_status("这张已经不在壁纸库里了".to_string());
                        }
                    }
                },
                // 大图那一层关掉了:把那份像素放掉(它在面板线程里占着几 MB 的 RGBA)
                mcwp_ui::UiEvent::ViewerClosed => {
                    if let Some(handle) = &panel {
                        handle.clear_viewer();
                    }
                }
                // 预览图右键「设为壁纸」:生图不再自动换壁纸之后,这就是"我想用这张"的入口。
                // 走的还是库页点一张那条路(`switch_to_photo`):两件事本来就是同一件 ——
                // "把库里第 N 张换到桌面上",只是这次的 N 不是用户点出来的,而是会话里那张图给的。
                mcwp_ui::UiEvent::SetWallpaper(id) => {
                    match library.iter().position(|entry| entry.source.photo.id == id) {
                        Some(index) => {
                            match switch_to_photo(
                                &mut renderer,
                                &library,
                                index,
                                screen,
                                brightness,
                            ) {
                                Ok(photo) => {
                                    let title = photo.photo.title.clone();
                                    tracing::info!("设为壁纸:{title}(会话里的预览图右键)");
                                    active = Some(photo);
                                    if let Some(handle) = &panel {
                                        handle.refresh(panel_state(
                                            active.as_ref().map(|it| &it.photo),
                                            &sessions,
                                            &library,
                                        ));
                                        handle.set_status(format!("已设为壁纸「{title}」"));
                                    }
                                }
                                Err(err) => {
                                    tracing::warn!("设为壁纸失败:{err:?}");
                                    if let Some(handle) = &panel {
                                        let reason = brief(&format!("{err:#}"));
                                        handle.set_status(format!("这张打不开:{reason}"));
                                    }
                                }
                            }
                        }
                        // 菜单开着的时候图被删了(库里已经找不到)。说清楚,别静默吞掉。
                        None => {
                            tracing::warn!("设为壁纸:库里找不到 {id}");
                            if let Some(handle) = &panel {
                                handle.set_status("这张已经不在壁纸库里了".to_string());
                            }
                        }
                    }
                }
                // 库页工具栏「用当前壁纸」:把桌面上那张抬进来(和 `--adopt` 同一条路)。
                mcwp_ui::UiEvent::AdoptWallpaper => {
                    match adopt_desktop(&mut renderer, &mut library, screen, brightness) {
                        Ok(photo) => {
                            let title = photo.photo.title.clone();
                            tracing::info!("已把桌面上那张抬进来:{title}");
                            active = Some(photo);
                            if let Some(handle) = &panel {
                                handle.refresh(panel_state(
                                    active.as_ref().map(|it| &it.photo),
                                    &sessions,
                                    &library,
                                ));
                                handle.set_status(format!("已把桌面上那张抬进来:{title}"));
                            }
                        }
                        Err(err) => {
                            tracing::warn!("抬桌面壁纸失败:{err:?}");
                            if let Some(handle) = &panel {
                                let reason = brief(&format!("{err:#}"));
                                handle.set_status(format!("没读到桌面壁纸:{reason}"));
                            }
                        }
                    }
                }
                mcwp_ui::UiEvent::Closed => {
                    if let Some(mut closed) = panel.take() {
                        closed.wait();
                    }
                    tracing::info!("控制面板已关闭");
                }
                // 面板切到「壁纸库」页时问一次,库目录被外面改过(手工删、拷进来)也能跟上。
                mcwp_ui::UiEvent::RefreshLibrary => {
                    library = scan_library();
                    if let Some(handle) = &panel {
                        // 库目录可能被外面动过,所以连会话一起回填:某条消息的配图可能刚被删了
                        handle.refresh(panel_state(
                            active.as_ref().map(|it| &it.photo),
                            &sessions,
                            &library,
                        ));
                    }
                }
                // 点库里的某一张:现解一次图换上。和托盘"换一张"走同一个入口(`switch_to_photo`)。
                mcwp_ui::UiEvent::OpenWallpaper(index) => {
                    match switch_to_photo(&mut renderer, &library, index, screen, brightness) {
                        Ok(photo) => {
                            let title = photo.photo.title.clone();
                            tracing::info!("切换到壁纸库里第 {index} 张:{title}");
                            active = Some(photo);
                            if let Some(handle) = &panel {
                                handle.refresh(panel_state(
                                    active.as_ref().map(|it| &it.photo),
                                    &sessions,
                                    &library,
                                ));
                                handle.set_status(format!("已切到 {title}"));
                            }
                        }
                        Err(err) => {
                            tracing::warn!("切到第 {index} 张失败:{err:?}");
                            if let Some(handle) = &panel {
                                handle.set_status(format!(
                                    "这张打不开:{}",
                                    brief(&format!("{err:#}"))
                                ));
                            }
                        }
                    }
                }
                // 「做成动态壁纸」:库格子的右键菜单带下标,库页工具栏那颗钮认**当前那张**。
                mcwp_ui::UiEvent::MakeVideo(index) => {
                    video_id += 1;
                    let status = start_video_run_visible(
                        &ai,
                        &videos,
                        &library,
                        index,
                        screen,
                        &video_tx,
                        &mut video_run,
                        video_id,
                        &mut video_target,
                        panel.as_ref(),
                    );
                    tracing::info!("做成动态壁纸(库第 {index} 张):{status}");
                    if let Some(handle) = &panel {
                        handle.set_status(status);
                    }
                }
                // 会话里那条回复下面的「生成动图」:面板递的是**那张图的 id**。
                // 在这儿才换成库下标 —— 面板不知道"这在库里是第几张",而"它还在不在库里"
                // 也只有这边说了算(消息是历史,那张图可能已经被用户在库页里删掉了)。
                mcwp_ui::UiEvent::MakeVideoFromMessage(id) => {
                    let index = library.iter().position(|entry| entry.source.photo.id == id);
                    let status = match index {
                        Some(index) => {
                            video_id += 1;
                            start_video_run_visible(
                                &ai,
                                &videos,
                                &library,
                                index,
                                screen,
                                &video_tx,
                                &mut video_run,
                                video_id,
                                &mut video_target,
                                panel.as_ref(),
                            )
                        }
                        None => {
                            "这张已经不在壁纸库里了 —— 库页里点它一下,或者重新生成一张".to_string()
                        }
                    };
                    tracing::info!("做成动态壁纸(会话里那条 {id}):{status}");
                    if let Some(handle) = &panel {
                        handle.set_status(status);
                    }
                }
                // 「免费取回」:照上次失败留下的任务号把成片捡回来。
                //
                // 这条路**只轮询 + 下载**,不提交新任务 —— 服务商那边已经计过费,
                // 重提就是又花一次钱。取回失败时任务号仍然留着,用户还能再点一次。
                mcwp_ui::UiEvent::FetchVideo { task, photo } => {
                    let status = if video_run.is_some() {
                        "正在做上一段动态壁纸,等它出来再说".to_string()
                    } else if let Some(animator) = videos.as_ref() {
                        match library
                            .iter()
                            .position(|entry| entry.source.photo.id == photo)
                        {
                            Some(index) => {
                                video_id += 1;
                                let run_id = video_id;
                                video_run = Some(run_id);
                                video_target = Some(photo.clone());
                                if let Some(handle) = &panel {
                                    handle.set_animating(photo.clone());
                                }
                                let sender = video_tx.clone();
                                let animator = animator.clone();
                                let entry = library[index].source.clone();
                                let source_id = photo.clone();
                                let task_id = task.clone();
                                std::thread::spawn(move || {
                                    let result = match fetch_video_payload(
                                        &animator, &entry, &task_id, screen,
                                    ) {
                                        Ok(payload) => Ok(Box::new(payload)),
                                        Err(err) => Err(VideoFailure {
                                            message: format!("{err:#}"),
                                            // 取回失败时任务号仍然有效,留着再试
                                            task_id: Some(task_id),
                                            source_id,
                                        }),
                                    };
                                    let _ = sender.send(VideoOutcome {
                                        run: run_id,
                                        result,
                                    });
                                });
                                format!("正在按任务号免费取回(不提交、不再扣费):{task}")
                            }
                            None => "这张已经不在壁纸库里了 —— 没法取回".to_string(),
                        }
                    } else {
                        "还没配图生视频的 key —— 它和生图是同一把,在「设置」里填上就能用"
                            .to_string()
                    };
                    tracing::info!("免费取回(任务 {task},图 {photo}):{status}");
                    if let Some(handle) = &panel {
                        handle.set_status(status);
                    }
                }
                mcwp_ui::UiEvent::MakeVideoFromCurrent => {
                    // 「当前这张」得先在库里找到它的位置 —— 找不到就没得做:
                    // 图生视频要的是库里那张**原图的字节**,而"当前这张"可能才刚抬进来、
                    // 还没进库(那种情况下连它在盘上哪儿都不知道)。
                    let index = active.as_ref().and_then(|it| {
                        library
                            .iter()
                            .position(|entry| entry.source.photo.id == it.photo.id)
                    });
                    let status = match index {
                        Some(index) => {
                            video_id += 1;
                            start_video_run_visible(
                                &ai,
                                &videos,
                                &library,
                                index,
                                screen,
                                &video_tx,
                                &mut video_run,
                                video_id,
                                &mut video_target,
                                panel.as_ref(),
                            )
                        }
                        None => "这一张还不在壁纸库里,先去库页点它一下再回来".to_string(),
                    };
                    tracing::info!("做成动态壁纸(当前这张):{status}");
                    if let Some(handle) = &panel {
                        handle.set_status(status);
                    }
                }
                // 设置页点了保存:写 ai.json + 就地重建客户端,不用重启程序。
                mcwp_ui::UiEvent::SaveAiConfig {
                    base_url,
                    model,
                    api_key,
                    thinking,
                    image_base_url,
                    image_model,
                    image_api_key,
                    image_quality,
                    image_watermark,
                    video_model,
                } => {
                    // **先从盘上现读一份**再覆盖回调带来的这几个字段:直接构造一个 `Stored` 会把盘上
                    // 这一版还不认识的字段(以后加的)悄悄抹掉 —— 那种丢失没有报错、没有提示,
                    // 是设置页最恶劣的一类 bug。
                    let mut stored = mcwp_ai::Config::stored();
                    // 一律 trim:从输入框里复制粘贴十有八九带空格/换行,而 key 多一个空格就是 401
                    stored.base_url = base_url.trim().to_string();
                    stored.model = model.trim().to_string();
                    stored.api_key = api_key.trim().to_string();
                    stored.image_base_url = image_base_url.trim().to_string();
                    stored.image_model = image_model.trim().to_string();
                    stored.image_api_key = image_api_key.trim().to_string();
                    stored.image_quality = image_quality.trim().to_string();
                    // 面板那颗胶囊永远是个明确的二选一,所以这里写 `Some` ——
                    // 落盘成 `true` 就是"我就是要带水印",重开程序也还在。
                    stored.image_watermark = Some(image_watermark);
                    stored.video_model = video_model.trim().to_string();
                    stored.thinking = thinking.to_option();
                    let status = save_ai_config(&stored, &mut ai, &mut images, &mut videos);
                    if let Some(handle) = &panel {
                        // 回填一次:trim 之后的值要显示回来,用户才知道真存进去的是什么
                        handle.refresh_ai(ai_config_state());
                        handle.set_status(status);
                    }
                }
            }
        }

        // —— 生图的结果落地 ——
        // 只有会话还在才继续:用户可能在等的时候把这条删了,那就别再往库里塞一张、也别在
        // 那儿留一条回复(删了还冒出来,比不冒出来更让人困惑)。
        while let Ok(outcome) = ai_rx.try_recv() {
            let AiOutcome {
                session,
                run: outcome_run,
                result,
            } = outcome;
            // 认轮次:暂停过、或者用户又发了新的一句之后,旧请求的结果会晚一步回来 —— 扔掉。
            if !matches!(&run, Some(Run::Running { id, .. }) if *id == outcome_run) {
                tracing::info!("丢掉过期的一轮(第 {outcome_run} 轮已经不是当前这一轮)");
                continue;
            }
            // 落地之后就不在跑了 —— 这颗钮必须回到「发送」,漏了它就永远停在暂停键上
            run = None;
            if let Some(handle) = &panel {
                handle.set_run_state(mcwp_ui::RunState::Idle);
            }
            if !sessions.exists(session) {
                tracing::info!("这一轮的会话已经被删掉了,结果不再落地");
                continue;
            }
            // 第三个是"这条回复配的那张壁纸"在库里的 id:面板要在回复下面把图贴出来
            // (点一下看大图)。没画出来 / 没上屏就是 `None`。
            let (reply, status, photo_id) = match result {
                Ok(payload) => {
                    let calls = payload.calls;
                    let degraded = payload.degraded;
                    let payload_watermarked = payload.watermarked;
                    let reply = payload.reply.clone();
                    // **只入库,不自动换壁纸**(见 `save_generated_photo`)。所以 `active` 这里
                    // 一个字都不动:桌面还是原来那张,什么时候换由用户在预览图的右键菜单里说。
                    match save_generated_photo(
                        &mut renderer,
                        &mut library,
                        *payload,
                        screen,
                        brightness,
                    ) {
                        Ok(saved) => {
                            let title = saved.title;
                            tracing::info!(
                                "本轮出图:{title} | 提示词调用={calls} | 降级={degraded} | 自动上屏={}",
                                if saved.applied_as_fallback {
                                    "是(库写不进去)"
                                } else {
                                    "否"
                                }
                            );
                            let status = if saved.applied_as_fallback {
                                format!("已画好「{title}」· 库写不进去,先直接挂上了")
                            } else if payload_watermarked {
                                // 兜底重发过:用户看到的是"明明关了水印怎么还有角标",
                                // 所以这一句必须是状态行里最先看到的那句,并带上原因和出口。
                                format!(
                                    "已画好「{title}」放进壁纸库(这张带「AI生成」角标:账号还没签去水印声明)\
                                     · 右键预览图可设为壁纸"
                                )
                            } else if degraded {
                                format!("模型没接上 · 用你的原话画了「{title}」,已放进壁纸库")
                            } else if calls > 1 {
                                format!(
                                    "已画好「{title}」放进壁纸库(提示词重试过一次) · 右键预览图可设为壁纸"
                                )
                            } else {
                                format!("已画好「{title}」放进壁纸库 · 右键预览图可设为壁纸")
                            };
                            (reply, status, Some(saved.id))
                        }
                        Err(err) => {
                            tracing::error!("图出来了但没能留住:{err:?}");
                            let reason = brief(&format!("{err:#}"));
                            (reply, format!("图出来了但没能入库:{reason}"), None)
                        }
                    }
                }
                Err(reason) => {
                    tracing::warn!("这一轮生图失败:{reason}");
                    let reason = brief(&reason);
                    (
                        format!("这次没画出来({reason}),再试一次?"),
                        format!("生图失败:{reason}"),
                        None,
                    )
                }
            };
            if !sessions.reply_with_photo(session, &reply, photo_id.as_deref()) {
                continue;
            }
            if let Some(handle) = &panel {
                handle.refresh(panel_state(
                    active.as_ref().map(|it| &it.photo),
                    &sessions,
                    &library,
                ));
                handle.set_status(status);
            }
        }

        // 会话改过就落盘。放在这儿而不是每个动作里各写一遍:一帧里可能连着好几笔改动
        // (发消息 + 切会话),这里只落最后一次;`flush` 自己会看有没有改过。
        sessions.flush();

        // —— 动态壁纸的结果落地 ——
        // 和生图那边同一个套路:主循环只收结果,不等。这一段跑的时候用户可能已经切走、
        // 甚至把库翻了个遍 —— 所以成功之后一律刷新面板(库多了一条新的)。
        while let Ok(outcome) = video_rx.try_recv() {
            let VideoOutcome {
                run: outcome_run,
                result,
            } = outcome;
            // 认轮次:理论上同一时刻只有一轮(起跑前拦过),但这条通道将来要是放开了并发,
            // 这一步就是"旧结果别把新的一轮盖掉"的唯一保障。
            if video_run != Some(outcome_run) {
                tracing::info!("丢掉过期的动态壁纸结果(第 {outcome_run} 轮)");
                continue;
            }
            video_run = None;
            // 那条回复下面的「生成动图」要回到原样:成功时它会变成"已是动图"(下面那次刷新
            // 会带上新的库数据),失败时它得能再点一次 —— 两种落点都要求先把"正在做"清掉。
            // 目标 id 先留下来:失败时要把任务号写回会话里对应的那张图。
            let target = video_target.take();
            let status = match result {
                Ok(payload) => {
                    // 成片有了,这条失败记录就该清掉 —— 留着只会让旧任务号继续占着按钮。
                    let origin = if payload.photo.origin_id.is_empty() {
                        target.unwrap_or_default()
                    } else {
                        payload.photo.origin_id.clone()
                    };
                    if !origin.is_empty() {
                        sessions.set_video_task(&origin, None);
                    }
                    let title = payload.photo.title.clone();
                    let seconds = payload.photo.duration_ms as f32 / 1000.0;
                    match adopt_video(&mut renderer, &mut library, *payload, screen, brightness) {
                        Ok(photo) => {
                            tracing::info!("动态壁纸已上屏:{title} · {seconds:.1}s");
                            active = Some(photo);
                            format!("已换上动态壁纸「{title}」({seconds:.1} 秒,循环播放)")
                        }
                        Err(err) => {
                            tracing::error!("动态壁纸出来了但上不了屏:{err:?}");
                            format!("视频做出来了但上不了屏:{}", brief(&format!("{err:#}")))
                        }
                    }
                }
                Err(failure) => {
                    // 有任务号就存回会话:那条回复下面会立刻变成「免费取回」。
                    // 没有任务号的失败(还没提交成功)不动旧任务号 —— 那一条也许还能取回。
                    if let Some(task) = failure.task_id.as_deref() {
                        sessions.set_video_task(&failure.source_id, Some(task));
                    }
                    tracing::warn!("做成动态壁纸失败:{}", failure.message);
                    let hint = if failure.task_id.is_some() {
                        " · 那条回复下面可以「免费取回」"
                    } else {
                        ""
                    };
                    format!("没做成动态壁纸:{}{hint}", brief(&failure.message))
                }
            };
            if let Some(handle) = &panel {
                handle.refresh(panel_state(
                    active.as_ref().map(|it| &it.photo),
                    &sessions,
                    &library,
                ));
                handle.set_animating("");
                handle.set_status(status);
            }
        }

        // —— 托盘菜单的落地 ——
        let mut quitting = false;
        for action in tray::drain_actions() {
            match action {
                TrayAction::OpenPanel => {
                    if panel.is_some() {
                        // 窗口是 UI 线程异步建的,极短时间内可能还找不到,重试交给下一次点击。
                        if !raise_panel() {
                            tracing::warn!("控制面板还没建好窗口,稍后再点一次");
                        }
                    } else {
                        library = scan_library();
                        panel = open_panel(
                            active.as_ref().map(|it| &it.photo),
                            &sessions,
                            &library,
                            &ui_tx,
                        );
                    }
                }
                TrayAction::NextPreset => {
                    // 托盘「换一张」= 库里的下一张(走到头绕回第一张)。库空着就没得换 ——
                    // 那种情况连"当前是哪张"都无从谈起,什么都不做比乱切一张强。
                    if !library.is_empty() {
                        wanted_photo =
                            Some((current_photo_index(&active, &library) + 1) % library.len());
                    }
                }
                TrayAction::TogglePause => {
                    paused = !paused;
                    tray::set_paused(paused);
                    tracing::info!("{}渲染", if paused { "暂停" } else { "继续" });
                }
                TrayAction::ToggleAutostart => {
                    let target = !mcwp_platform::autostart::is_enabled();
                    if let Err(err) = mcwp_platform::autostart::set_enabled(target) {
                        tracing::error!("切换开机自启失败:{err:?}");
                    }
                }
                TrayAction::Quit => {
                    tracing::info!("收到托盘退出");
                    quitting = true;
                }
            }
        }

        if let Some(index) = wanted_photo {
            match switch_to_photo(&mut renderer, &library, index, screen, brightness) {
                Ok(photo) => {
                    tracing::info!("托盘换了一张:{}", photo.photo.title);
                    active = Some(photo);
                    if let Some(handle) = &panel {
                        handle.refresh(panel_state(
                            active.as_ref().map(|it| &it.photo),
                            &sessions,
                            &library,
                        ));
                    }
                }
                Err(err) => tracing::warn!("换一张失败:{err:?}"),
            }
        }

        if quitting {
            break;
        }

        if let Some(limit) = args.seconds {
            if started.elapsed().as_secs_f32() >= limit {
                tracing::info!("达到自检时长,退出");
                break;
            }
        }

        let frame_start = Instant::now();
        if !paused {
            // 这一帧**实际**过了多久。视频要拿它算"该换下一帧了吗" ——
            // 用"应该在多久之后"去推进时钟,掉帧的时候视频会偷偷跑快。
            let delta = (frame_start - last_tick).as_secs_f32();
            clock += delta;
            // 有照片就画照片。**没有可画的东西时一帧都不画**:`Renderer::new` 已经铺过一块
            // 认得出是"还没画"的占位色,继续重画同一帧只是白烧 GPU 和电。
            if let Some(photo) = &mut active {
                photo.params.time = clock;
                // 视频:该换帧了就把新的一帧贴上去(`tick` 自己按视频帧率判断)。
                // 它和"每帧都画一次"是两件事 —— 桌面 60fps,视频只有 31fps。
                let failure = match photo.video.as_mut() {
                    Some(video) => video.tick(&mut renderer, delta).err(),
                    None => None,
                };
                if let Some(err) = failure {
                    // 放到一半出错(文件被删了、被别的程序占住了)不该让整个程序退出:
                    // 丢掉播放器,画面停在最后一帧。用户想换一张就右键换一张。
                    tracing::warn!("视频放不下去了({err:?}),先停在当前这一帧");
                    photo.video = None;
                }
                // 曝光校正在 `VideoPlayback::tick` 里烘进视频像素,不能再走渲染器的全局亮度:
                // 否则最后一帧已经淡回首帧,还会被再乘一次曝光增益,循环点反而多出一记亮度跳。
                photo.params.brightness = photo.base_brightness;
                renderer.render(&photo.params)?;
                frames_in_window += 1;
                rendered_total += 1;
            }
        }
        last_tick = frame_start;

        if last_stats.elapsed() >= args.stats_interval {
            let elapsed = last_stats.elapsed().as_secs_f64();
            tracing::info!(
                "fps={:.1} 渲染帧={} 跳过帧={} 分辨率={:?} 写参数={:.1}ms 取帧={:.1}ms 提交={:.1}ms 呈现={:.1}ms",
                frames_in_window as f64 / elapsed,
                rendered_total,
                renderer.skipped_frames,
                renderer.size(),
                renderer.last_write_ms,
                renderer.last_acquire_ms,
                renderer.last_submit_ms,
                renderer.last_present_ms
            );
            let (ws, private) = memory_mb();
            tracing::info!("内存:工作集={ws:.1}MB 私有={private:.1}MB");
            frames_in_window = 0;
            last_stats = Instant::now();
        }

        let spent = frame_start.elapsed();
        if spent < frame_budget {
            std::thread::sleep(frame_budget - spent);
        }
    }

    tracing::info!(
        "退出,共渲染 {rendered_total} 帧,跳过 {} 帧",
        renderer.skipped_frames
    );
    // 收尾再落一次:自检时长到点、托盘退出这些路径都是从循环里直接跳出来的。
    sessions.flush();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 接缝混合的强度曲线。
    ///
    /// 这条曲线**肉眼看不出来**(它就是"最后 0.8 秒慢慢并回第一帧"),只能靠测试钉住两端:
    /// 窗口之前必须是 0(否则整段都在往回并),末尾必须到 1(否则那一记硬切还在)。
    #[test]
    fn 接缝淡入两端是零和一() {
        let duration = 10.0;
        assert_eq!(loop_fade_alpha(0.0, duration), 0.0);
        assert_eq!(loop_fade_alpha(9.0, duration), 0.0, "还没进窗口,不该淡");
        assert_eq!(
            loop_fade_alpha(duration - LOOP_FADE_SECS, duration),
            0.0,
            "窗口的第一帧是从 0 开始的,不能一上来就有强度"
        );
        let middle = loop_fade_alpha(9.6, duration);
        assert!(middle > 0.0 && middle < 1.0, "窗口中间是半淡:{middle}");
        assert!(
            (loop_fade_alpha(duration, duration) - 1.0).abs() < 1e-6,
            "末尾必须完全等于第一帧"
        );
    }

    /// 强度只能往上走:中途回落会让画面"弹"一下,比硬切还难看。
    #[test]
    fn 接缝淡入是单调的() {
        let duration = 10.0;
        let mut previous = 0.0f32;
        for step in 0..=200 {
            let at = 9.2 + LOOP_FADE_SECS * (step as f32 / 200.0);
            let value = loop_fade_alpha(at, duration);
            assert!(
                value >= previous - 1e-6,
                "在 {at}s 处回落了:{value} < {previous}"
            );
            previous = value;
        }
    }

    /// 开头也必须从首帧展开:结尾淡回首帧只接上了“末帧→首帧”,
    /// 第一帧到第二个视频帧仍是整段最大的一步跳变。
    #[test]
    fn 接缝开头从首帧展开() {
        let duration = 10.0;
        assert!((loop_intro_alpha(0.0, duration) - 1.0).abs() < 1e-6);
        assert_eq!(loop_intro_alpha(LOOP_FADE_SECS, duration), 0.0);
        let middle = loop_intro_alpha(LOOP_FADE_SECS / 2.0, duration);
        assert!(middle > 0.0 && middle < 1.0, "窗口中间是半淡:{middle}");

        let mut previous = 1.0f32;
        for step in 0..=100 {
            let at = LOOP_FADE_SECS * (step as f32 / 100.0);
            let value = loop_intro_alpha(at, duration);
            assert!(
                value <= previous + 1e-6,
                "在 {at}s 处回升了:{value} > {previous}"
            );
            previous = value;
        }
    }

    /// 曝光增益:末帧比首帧亮多少,就乘多少回来;差得太小、或者量出离谱的比值,一律不校。
    #[test]
    fn 曝光增益按首末帧的亮度算() {
        // 末帧整体比首帧亮 10% → 播到头要把画面压掉约 9%
        let first = vec![100u8, 100, 100, 255];
        let last = vec![110u8, 110, 110, 255];
        let gain = seam_gain_for(&first, &last);
        assert!((gain - 0.909).abs() < 0.01, "算出来是 {gain}");
        // 两帧一样亮 → 不校(1.0),别为看不出的事每帧多算一次
        assert_eq!(seam_gain_for(&first, &first), 1.0);
        // 坏输入:空帧、长度不一致,都不该让画面变黑
        assert_eq!(seam_gain_for(&[], &last), 1.0);
        assert_eq!(seam_gain_for(&first, &[]), 1.0);
        assert_eq!(seam_gain_for(&first, &[200u8, 200]), 1.0);
        // 比值离谱(这里是 10 倍)→ 夹到 1.35,而不是照乘
        assert_eq!(
            seam_gain_for(&[200u8, 200, 200, 255], &[20u8, 20, 20, 255]),
            1.35
        );
    }

    /// 播放端的曝光增益只允许把暗视频提亮,绝不允许把亮视频压暗 ——
    /// 旧版拿首末帧比值摊到整段,把 0.75 乘满十秒,用户看到的就是"怎么这么暗"。
    #[test]
    fn 播放曝光只提不压() {
        assert_eq!(display_gain_for(None, 44.0), 1.0);
        assert_eq!(
            display_gain_for(Some(30.0), 44.0),
            1.0,
            "原图更暗时不压视频"
        );
        let gain = display_gain_for(Some(51.0), 44.0);
        assert!((gain - 1.159).abs() < 0.01, "算出来是 {gain}");
        assert_eq!(
            display_gain_for(Some(200.0), 44.0),
            1.35,
            "离谱的比值要夹住"
        );
    }

    /// 提亮只动 RGB,alpha 通道必须原样保留。
    #[test]
    fn 曝光提亮不动_alpha() {
        let pixels = vec![100u8, 120, 140, 77, 0, 10, 250, 255];
        let out = scale_rgb(&pixels, 1.2);
        assert_eq!(&out[0..3], &[120, 144, 168]);
        assert_eq!(out[3], 77);
        assert_eq!(&out[4..7], &[0, 12, 255]);
        assert_eq!(out[7], 255);
    }

    /// 接缝的最后一帧必须**严格等于首帧**,不能再被曝光增益乘一次。
    ///
    /// 这是循环点亮度硬跳的根因:原来先把末帧淡成首帧,再走渲染器全局亮度乘 0.75,
    /// 屏幕上实际是"首帧 × 0.75";下一轮立刻恢复成"首帧 × 1.0",于是每十秒闪一下。
    #[test]
    fn 接缝末帧不会被曝光增益二次压暗() {
        let first = vec![100u8, 120, 140, 255];
        let video = vec![240u8, 200, 160, 255];
        let mut out = Vec::new();
        let changed = compose_video_frame(&video, &first, 1.0, 0.75, &mut out);
        assert!(changed);
        assert_eq!(out, first, "alpha=1 时输出必须等于首帧原像素");

        // 没到接缝时,增益仍然作用于视频帧:末帧更亮就该压暗。
        let changed = compose_video_frame(&video, &first, 0.0, 0.75, &mut out);
        assert!(changed);
        assert_eq!(out, vec![180u8, 150, 120, 255]);

        // 既不用混、也不用校 → 不拷帧,直接走原像素那条快路。
        assert!(!compose_video_frame(&video, &first, 0.0, 1.0, &mut out));
    }

    /// 短片别整段都在淡 —— 那等于整段都不对劲,所以窗口按片子长度的 1/4 收窄。
    #[test]
    fn 短片的淡入窗口会收窄() {
        // 1 秒的片子:窗口是 0.25 秒(不是 0.8 秒),所以 0.5 秒处还没开始淡
        assert_eq!(loop_fade_alpha(0.5, 1.0), 0.0);
        assert!(loop_fade_alpha(0.9, 1.0) > 0.0);
        assert!((loop_fade_alpha(1.0, 1.0) - 1.0).abs() < 1e-6);
        // 时长为 0 / 解不出长度的坏输入:不淡,照原样放
        assert_eq!(loop_fade_alpha(0.0, 0.0), 0.0);
        assert_eq!(loop_fade_alpha(5.0, 0.0), 0.0);
    }

    fn guide_test_bitmap(width: u32, height: u32) -> mcwp_image::Bitmap {
        let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height {
            for x in 0..width {
                let value = if y < height / 3 {
                    180 + (x * 7 % 50) as u8
                } else {
                    70 + (x * 5 + y * 3) as u8 % 120
                };
                rgba.extend_from_slice(&[value, value, value, 255]);
            }
        }
        mcwp_image::Bitmap {
            width,
            height,
            rgba,
        }
    }

    fn guide_test_plan() -> mcwp_spec::MotionPlan {
        mcwp_spec::MotionPlan {
            camera: "固定机位".to_string(),
            elements: vec![
                mcwp_spec::MotionElement {
                    kind: "clouds".to_string(),
                    area: "sky".to_string(),
                    action: "stream left".to_string(),
                    sentence: "Clouds stream left across the sky.".to_string(),
                },
                mcwp_spec::MotionElement {
                    kind: "grass".to_string(),
                    area: "foreground".to_string(),
                    action: "bend".to_string(),
                    sentence: "Grass bends in the wind.".to_string(),
                },
            ],
            caption: String::new(),
            scene: "A field under a cloudy sky.".to_string(),
        }
    }

    /// 尾帧只允许动局部元素:四角和画面中央的“路”必须保持原像素。
    #[test]
    fn 动效尾帧只动局部元素() {
        let bitmap = guide_test_bitmap(90, 60);
        let (guide, warps, max_shift) = motion_guide_bitmap(&bitmap, &guide_test_plan());
        assert!(warps >= 2, "天空和草地都该有引导区:{warps}");
        assert!(max_shift > 0.0, "至少得有一个像素级的位移:{max_shift}");

        let pixel = |image: &mcwp_image::Bitmap, x: usize, y: usize| {
            let at = (y * image.width as usize + x) * 4;
            image.rgba[at..at + 4].to_vec()
        };
        assert_eq!(pixel(&guide, 0, 0), pixel(&bitmap, 0, 0), "左上角不能动");
        assert_eq!(
            pixel(&guide, 45, 50),
            pixel(&bitmap, 45, 50),
            "画面中央的路不能动"
        );
        assert_ne!(
            pixel(&guide, 10, 10),
            pixel(&bitmap, 10, 10),
            "云层该有位移"
        );
        assert_ne!(
            pixel(&guide, 10, 50),
            pixel(&bitmap, 10, 50),
            "草地该有位移"
        );
    }

    /// 引导帧的局部位移必须足够大。旧值在 320 宽上只有 2~3 px,视频模型会当成静帧;
    /// 现在草地/蕨类应该到 5 px 以上(1080p 约 30 px)。
    #[test]
    fn 动效尾帧的位移足够让模型看见() {
        let bitmap = guide_test_bitmap(320, 180);
        let (_, _, max_shift) = motion_guide_bitmap(&bitmap, &guide_test_plan());
        assert!(max_shift >= 4.0, "位移太小,模型会当成静帧:{max_shift}");
    }

    /// 纯光影方案不生成尾帧:否则又回到“只有明暗变化”的老问题。
    #[test]
    fn 纯光效方案不生成尾帧() {
        let bitmap = guide_test_bitmap(60, 40);
        let plan = mcwp_spec::MotionPlan {
            camera: "固定机位".to_string(),
            elements: vec![mcwp_spec::MotionElement {
                kind: "Sunlight".to_string(),
                area: "全图".to_string(),
                action: "shift".to_string(),
                sentence: "Sunlight shifts across the room.".to_string(),
            }],
            caption: String::new(),
            scene: "A room.".to_string(),
        };
        let (guide, warps, max_shift) = motion_guide_bitmap(&bitmap, &plan);
        assert_eq!(warps, 0);
        assert_eq!(max_shift, 0.0);
        assert_eq!(guide, bitmap);
    }
}
