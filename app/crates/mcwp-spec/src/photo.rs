//! MC 壁纸 · 照片壁纸(`Photo`)—— 「生图」那条路上的数据模型。
//!
//! 和 `WallpaperSpec` 是**两代东西**,刻意分成两个模块,而不是给同一个结构加字段:
//! - `WallpaperSpec` 是"画面全部由本地着色器现算"的时代。它描述的其实是一个**程序**:
//!   预设 id + 参数,WGSL 拿着它们算出每一帧。所以它 1~3 KB、可以任意分享、每次复现都一样。
//! - `Photo` 描述的是**一张已经生好的图**。JSON 里是文件名、提示词、动效配方,画面本身是那张
//!   PNG。它**没有"复现"的义务** —— 同一个提示词再跑一次是另一张图,这是生图的本质。
//!   硬把它们塞进一个结构,就得给一半字段写"这一支不用"的注释,而且每加一个生图参数都要
//!   重新审一遍旧预设的兼容性。
//!
//! 为什么"图"和"它的 JSON"是两个文件,而不是把 JSON 塞进 PNG 的 tEXt 块:
//! - 塞进去确实更整洁(拷一个文件就是一张壁纸),但**读元数据就得先解 PNG**。面板每刷新一次
//!   库就要扫一遍,那会从"读几十个几 KB 的 JSON"变成"解几十张 1080p 图",每次刷新多几百毫秒。
//! - 用户手工改提示词 / 改动效时,动一个文本文件比改 PNG 元数据现实得多。
//! - 两个文件同目录、同 id,各判各的:PNG 坏了不至于连标题都看不见,JSON 坏了也不至于丢图。

use serde::{Deserialize, Serialize};

use crate::PALETTE_SLOTS;

/// `Photo` 的结构版本。加字段不算破坏性改动,改语义时才 +1。
pub const PHOTO_SCHEMA: u32 = 1;

/// 调色板抽稀用的分块格子。24×14 ≈ 按 16:9 铺一层,每格再按 `SAMPLE_STEP` 跳采 ——
/// 1920×1080 上一共只读约 53 万个像素,几毫秒的事,而且"块平均色"本来就该糊一点。
const GRID_COLS: u32 = 24;
const GRID_ROWS: u32 = 14;
/// 块内采样步长(像素)。取平均色不怕跳采,步长 2 直接省掉 3/4 的读取。
const SAMPLE_STEP: u32 = 2;

/// 提不出颜色时的兜底(中性冷灰蓝)。面板宁可素一点,也不能变成一片纯黑或纯白。
const FALLBACK_PALETTE: [&str; PALETTE_SLOTS] =
    ["#14161C", "#22252E", "#4F7CFF", "#6E7686", "#AEB4C0"];

/// 动效种类 —— 一张静图"活"起来的最小集合。
///
/// 分两档,理解这一点对调参很重要:
/// - `Still` 是**画面本身的运动**:整张图在缓慢推近 / 平移(Ken Burns)。它不改变图的观感,
///   只是让照片"在呼吸",所以任何题材都适用,也是所有壁纸的默认底子。
/// - 其余几个是在图**之上叠一层程序化效果**(雪 / 雨 / 雾 / 云影 / 光晕 / 水波)。它们会明显
///   改变观感,所以由提示词智能体按题材挑,不硬套。
///
/// 真正的"图生视频"(整帧都是新画面)是另一条路,不在这里凑数 —— 它产出的是视频文件而不是
/// 一张图 + 一层效果,塞进同一个枚举只会让下游到处写 `if`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MotionKind {
    /// 只有缓慢推拉 + 鼠标视差 —— 最克制,也最像"会呼吸的照片"
    #[default]
    Still,
    /// 雪粒
    Snow,
    /// 雨丝
    Rain,
    /// 雾气(低频噪声缓慢翻滚)
    Fog,
    /// 云影(大块明暗横扫)
    Cloud,
    /// 光晕(亮斑随呼吸明灭)
    Glow,
    /// 水波(横向正弦扰动,像隔着一层水看)
    Ripple,
}

impl MotionKind {
    /// 全部可选值,顺序就是面板上"动效"下拉框的顺序。
    pub const ALL: [MotionKind; 7] = [
        MotionKind::Still,
        MotionKind::Snow,
        MotionKind::Rain,
        MotionKind::Fog,
        MotionKind::Cloud,
        MotionKind::Glow,
        MotionKind::Ripple,
    ];

    /// 落盘 / 给模型认的 id(`snake_case`,和 `serde` 的表示一致,别只改一处)。
    pub fn id(self) -> &'static str {
        match self {
            MotionKind::Still => "still",
            MotionKind::Snow => "snow",
            MotionKind::Rain => "rain",
            MotionKind::Fog => "fog",
            MotionKind::Cloud => "cloud",
            MotionKind::Glow => "glow",
            MotionKind::Ripple => "ripple",
        }
    }

    /// 给用户看的名字。
    pub fn label(self) -> &'static str {
        match self {
            MotionKind::Still => "静帧呼吸",
            MotionKind::Snow => "落雪",
            MotionKind::Rain => "雨丝",
            MotionKind::Fog => "雾气",
            MotionKind::Cloud => "云影",
            MotionKind::Glow => "光晕",
            MotionKind::Ripple => "水波",
        }
    }

    /// 认不出来就回默认 —— 用户手改 JSON 写错单词时,不该让整张壁纸打不开。
    pub fn from_id(raw: &str) -> Option<Self> {
        let key = raw.trim().to_ascii_lowercase();
        Self::ALL.into_iter().find(|kind| kind.id() == key)
    }
}

/// **故意手写 `Deserialize`,不用 derive**:派生出来的实现遇到没见过的名字会直接报错,
/// 于是"JSON 里写了一个将来才有的动效名"会让整张壁纸打不开。这里的取舍是
/// **认不出来就退回静帧呼吸** —— 最坏情况是"动效没生效",而不是"壁纸没了"。
///
/// 注意它和 `Serialize` 那边靠 `#[serde(rename_all = "snake_case")]` 定名字这件事必须一致:
/// 两边的取值都是 `id()` 返回的那串。改名字记得改 `id()`,不是改这里。
impl<'de> Deserialize<'de> for MotionKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(Self::from_id(&raw).unwrap_or_default())
    }
}

/// 动效配方:这张图"怎么动"。
///
/// 只有 5 个旋钮,是**故意**的。生图那条路上,提示词智能体是唯一的生产者,而它是个小模型:
/// 旋钮越多、越容易给出"看起来合法其实难看"的组合(比如推得又快又狠,整张图在屏幕上抖)。
/// 所以每个旋钮都带一个保守的上限,`normalize()` 负责兜底。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MotionRecipe {
    pub kind: MotionKind,
    /// 效果层强度 0~1(`Still` 时无意义)
    pub intensity: f32,
    /// Ken Burns 推近幅度 0~1。0 = 完全不推;0.12 是"看得出在动但不会晕"的常用值
    pub zoom: f32,
    /// 平移幅度 0~1
    pub pan: f32,
    /// 速度 0~1。壁纸不该抢注意力,所以上限刻意压得低(见 `normalize`)
    pub speed: f32,
}

impl Default for MotionRecipe {
    fn default() -> Self {
        Self {
            kind: MotionKind::Still,
            intensity: 0.5,
            zoom: 0.12,
            pan: 0.06,
            speed: 0.3,
        }
    }
}

impl MotionRecipe {
    /// 速度的实际上限。0.6 大约是一分钟走完一个推拉来回 —— 再快就从"呼吸"变成"抽搐"了。
    pub const SPEED_MAX: f32 = 0.6;

    /// 夹紧所有旋钮。`NaN` / 无穷一律落到字段自己的默认值(模型偶尔会吐这种值)。
    pub fn normalize(&mut self) {
        self.intensity = clamp01(self.intensity).unwrap_or(0.5);
        self.zoom = clamp01(self.zoom).unwrap_or(0.12);
        self.pan = clamp01(self.pan).unwrap_or(0.06);
        self.speed = clamp_unit(self.speed, 0.02, Self::SPEED_MAX).unwrap_or(0.3);
    }

    /// 面板卡片上那一行小字。
    pub fn summary(&self) -> String {
        if self.zoom <= 0.001 && self.pan <= 0.001 {
            return format!("{} · 完全静止", self.kind.label());
        }
        format!(
            "{} · 推近 {:.0}% · 速度 {:.0}%",
            self.kind.label(),
            self.zoom * 100.0,
            self.speed * 100.0
        )
    }
}

/// `seam_gain` 的缺省值:**1.0 = 不校**。
///
/// 单独写成函数是因为 serde 的 `default` 要一个函数路径,而 f32 的 `Default` 是 0.0 ——
/// 那个值在这条路上意味着"整段画面乘 0",也就是全黑。
pub fn default_seam_gain() -> f32 {
    1.0
}

/// 画面里的**一个元素**,以及它该怎么动。
///
/// 为什么要有它:一张城市夜景要"活"起来,靠的是**灯在闪、车在走、云在飘**这些**分开的事**;
/// 让整张图缓慢推近,看起来只是"照片被放大了" —— 用户的原话是"不能仅仅依靠放大整张图片"。
///
/// `area` 是九宫格里的位置,不是像素坐标:生产者是视觉模型,它给不出可靠的像素框,
/// 硬要只会拿到一堆编出来的数;而"左上角那片灯"这种描述,人和模型都说得准。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MotionElement {
    /// 元素类型:灯光 / 车流 / 云 / 飞鸟 / 水面 / 衣摆 / 树叶 / 烟雾 / 人物…
    pub kind: String,
    /// 它在画面的哪儿:左上 / 上中 / 右上 / 左中 / 正中 / 右中 / 左下 / 下中 / 右下
    pub area: String,
    /// 它怎么动:亮灭闪烁 / 从左往右流动 / 缓慢飘移 / 被风吹起…
    pub action: String,
}

/// 一张图"动起来"的方案:**整体镜头 + 逐个元素**。
///
/// 它**只有一个消费者**:图生视频那句提示词(`mcwp_ai::video_prompt` 把 `camera` 和
/// 每个 `{area}{kind}{action}` 拼成一句话)。动效不在本地渲染层做 —— 那是被否掉的
/// 一条路(见 §11.23):画面里的东西该由画面自己动,不是在 shader 里再模拟一遍。
///
/// 值得落盘成资产,而不是用完就扔:**重新做一遍动态壁纸时不用再看一次图** ——
/// 那是一次模型调用,而且两次结果未必一样。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MotionPlan {
    /// 整体镜头:固定机位 / 缓慢推近 / 轻微环绕…
    pub camera: String,
    pub elements: Vec<MotionElement>,
}

impl MotionPlan {
    /// 最多认几个元素。
    ///
    /// 上限是**给提示词留的**:图生视频那句最长 512 字(接口限制,见 `mcwp_ai::MAX_VIDEO_PROMPT_CHARS`),
    /// 元素越多,每个分到的字越少,模型反而抓不住重点。四五个已经够"画面是活的"了。
    pub const MAX_ELEMENTS: usize = 5;

    /// 砍掉空字段、限制条数。视觉模型的返法很随意:偶尔漏字段、偶尔多写两倍的量。
    pub fn normalize(&mut self) {
        self.camera = self.camera.trim().to_string();
        self.elements.retain(|element| {
            !element.kind.trim().is_empty() && !element.action.trim().is_empty()
        });
        self.elements.truncate(Self::MAX_ELEMENTS);
        for element in &mut self.elements {
            element.kind = element.kind.trim().to_string();
            element.area = element.area.trim().to_string();
            element.action = element.action.trim().to_string();
        }
    }

    /// 一个元素都没有 —— 该退回"通用运镜"那种写法。
    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }
}

/// 「在画面的哪儿」的清单:九宫格 + 全图。
///
/// **这是那份提示词的唯一真相**:`mcwp_ai::VISION_SYSTEM_PROMPT` 里列的就是这十个,
/// 而且**顺序必须一致**(有条测试拿它去核提示词)。它不再喂给渲染器 ——
/// 位置现在只影响提示词怎么写(见 §11.23 末尾:动效归提示词,不归本地渲染层)。
pub const AREA_NAMES: [&str; 10] = [
    "左上", "上中", "右上", "左中", "正中", "右中", "左下", "下中", "右下", "全图",
];

/// 这张壁纸是**一张静图**,还是**一段视频**(由某张图生成的动态壁纸)。
///
/// 为什么要有这个字段,而不是"看目录里有没有 `.mp4` 就够了":
/// - 面板上两者的显示不一样(视频那格有个「动」标、摘要那一行写时长而不是推拉幅度);
/// - 渲染那条路上,"每帧只改参数"和"每帧换纹理"是两条路(见 `main.rs` 的 `VideoPlayback`);
/// - 元数据是要给人看的(手改 JSON、拷给别人),而"文件在不在"是**另一个**问题。
///
/// 但它**不是**唯一真相:扫库时以 `<id>.mp4` 在不在为准(见 `photos::read_entry`),
/// 免得出"文件删了、JSON 还写着 video"这种半拉状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhotoKind {
    /// 一张图:生图出来的,或者从桌面壁纸抬进来的
    #[default]
    Image,
    /// 一段视频:图生视频出来的
    Video,
}

impl PhotoKind {
    pub fn is_video(self) -> bool {
        matches!(self, PhotoKind::Video)
    }

    /// 给用户看的名字。
    pub fn label(self) -> &'static str {
        match self {
            PhotoKind::Image => "静图",
            PhotoKind::Video => "动态壁纸",
        }
    }
}

/// 一张照片壁纸。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Photo {
    pub schema: u32,
    /// 文件名主干的来源。图是 `<id>.png`,这份元数据是 `<id>.json`。
    pub id: String,
    pub title: String,
    /// 真正发给生图模型的那句提示词(中文)
    pub prompt: String,
    /// 生成时的实际像素。和屏幕比例不一定一致 —— 渲染时按 cover 裁,不裁文件。
    pub width: u32,
    pub height: u32,
    pub motion: MotionRecipe,
    /// 由这张图自己提取出来的 5 色(索引 0 最暗),面板玻璃和画面调色都吃它
    pub palette: Vec<String>,
    /// 随机盐。同一句提示词两次生成是两张图,所以它只用来让"同一张图"的效果层稳定复现。
    pub seed: u32,
    /// 生成时间(unix 秒),面板按它排序
    pub created: u64,
    /// 这是静图还是视频(见 `PhotoKind`)
    pub kind: PhotoKind,
    /// 视频多长(毫秒)。静图恒为 0 —— 面板上那一行"5.1 秒"就来自它。
    pub duration_ms: u32,
    /// 循环点的**曝光校正**:播放到结尾时该给画面乘的增益(1.0 = 不用校)。
    ///
    /// 为什么会有这个数:图生视频给的片子常常**一路变亮或变暗**(实测同一条片子的末帧
    /// 比首帧亮 10%),切回开头就是一次亮度跳。播放时按这个系数把整段的曝光慢慢拉回一致,
    /// 那点差就摊在十秒里,而不是堆在接缝那一瞬间(见 §11.22)。
    ///
    /// 它是**量出来的**,不是猜的:入库时把整段解一遍,拿首帧与末帧的平均亮度算比值。
    /// 老记录没有这个字段,缺省是 1.0(不校)—— 不能是 0.0,那会让整段画面全黑。
    #[serde(default = "default_seam_gain")]
    pub seam_gain: f32,
    /// 这张图"该怎么动"的**元素级**方案(见 `MotionPlan`)。
    ///
    /// 空的就是没分析过:从桌面壁纸抬进来的、以及这份字段存在之前生成的,都是空的。
    /// 落盘是为了让它当**资产**用:图生视频那句提示词吃它,而且"重新做成动态壁纸"时
    /// 不用再看一遍图(省一次调用,也省得两次结果不一样)。
    pub motion_plan: MotionPlan,
}

impl Default for Photo {
    fn default() -> Self {
        Self {
            schema: PHOTO_SCHEMA,
            id: String::new(),
            title: String::new(),
            prompt: String::new(),
            width: 0,
            height: 0,
            motion: MotionRecipe::default(),
            palette: Vec::new(),
            seed: 0,
            created: 0,
            kind: PhotoKind::Image,
            duration_ms: 0,
            motion_plan: MotionPlan::default(),
            seam_gain: default_seam_gain(),
        }
    }
}

impl Photo {
    /// 解析元数据。字段缺失补默认值,多余字段忽略(向前兼容,和 `WallpaperSpec::parse` 一个态度)。
    pub fn parse(text: &str) -> Result<Self, serde_json::Error> {
        let mut photo: Self = serde_json::from_str(text)?;
        photo.normalize();
        Ok(photo)
    }

    /// 造一个文件名安全的 id:`{时间戳:x}{提示词指纹:x}`,固定 16 个十六进制字符。
    ///
    /// 时间戳放前面是为了**一眼能按时间排**;指纹放后面是为了同一秒内连生两张也不会撞名
    /// (提示词不同 → 指纹不同)。反过来放不行:先看指纹会失去可读性,而且同一句提示词
    /// 连生两张会撞。
    pub fn new_id(prompt: &str, created: u64) -> String {
        let stamp = (created & 0xffff_ffff) as u32;
        let digest = crate::fnv1a64(prompt.trim().as_bytes()) as u32;
        format!("{stamp:08x}{digest:08x}")
    }

    /// 夹紧数值、补齐调色板与标题、缺 id 就补一个。
    pub fn normalize(&mut self) {
        self.schema = PHOTO_SCHEMA;
        self.title = self.title.trim().to_string();
        self.prompt = self.prompt.trim().to_string();
        if self.title.is_empty() {
            self.title = title_from_prompt(&self.prompt);
        }
        // 5 槽定长:多出来的丢掉,缺的用兜底色补 —— 下游(面板玻璃、着色器)都按定长读。
        let mut palette: Vec<String> = Vec::with_capacity(PALETTE_SLOTS);
        for (slot, fallback) in FALLBACK_PALETTE.iter().enumerate() {
            let value = self
                .palette
                .get(slot)
                .and_then(|raw| normalize_hex(raw))
                .unwrap_or_else(|| (*fallback).to_string());
            palette.push(value);
        }
        self.palette = palette;
        self.motion.normalize();
        self.motion_plan.normalize();
        // 没量过 / 量出来是坏值 → 不校(1.0)。**别当成"不用管"**:0.0 会让画面全黑,
        // 而它是 f32 的 `Default`,所以老记录、手写 JSON 都可能带一个 0 进来。
        if !self.seam_gain.is_finite() || self.seam_gain <= 0.0 {
            self.seam_gain = default_seam_gain();
        }
        if self.width == 0 {
            self.width = 1920;
        }
        if self.height == 0 {
            self.height = 1080;
        }
        if self.id.trim().is_empty() {
            self.id = Self::new_id(&self.prompt, self.created);
        }
        // 静图不该带时长:留着它会让面板显示"静图 · 5.1 秒"这种自相矛盾的话
        if !self.kind.is_video() {
            self.duration_ms = 0;
        }
    }

    pub fn to_pretty_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// 长宽比。着色器拿它把"图"贴满"屏"(cover):比例不同就裁两头,而不是加黑边 ——
    /// 壁纸加黑边比裁掉一点边难看得多。
    pub fn aspect(&self) -> f32 {
        if self.height == 0 {
            return 16.0 / 9.0;
        }
        self.width as f32 / self.height as f32
    }
}

/// 从提示词里搓一个短标题:取到第一个逗号 / 句号为止,再限长。
pub fn title_from_prompt(prompt: &str) -> String {
    let flat: String = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    let head = flat
        .split(['，', ',', '。', '.', '；', ';'])
        .next()
        .unwrap_or(&flat)
        .trim();
    let title: String = head.chars().take(14).collect();
    if title.is_empty() {
        "新壁纸".to_string()
    } else {
        title
    }
}

/// 从像素里提 5 个颜色,槽位含义和 `WallpaperSpec::palette` 完全一致
/// (0 底色 / 1 中间色 / 2 强调色 / 3 高光 / 4 最亮),这样面板玻璃和渲染器两条路共用一套读法。
///
/// 规则:
/// - 先把画面切成 24×14 块、各取平均色 —— 直接按像素排序会被噪点和渐变色带带偏,
///   按块平均得到的才是"这张图大致有哪些色"。
/// - 底色 / 中间色 / 高光 / 最亮**按亮度取分位**,保证整条调色板从深到浅,
///   玻璃那种"后面有张照片"的明暗层次才有得拉。
/// - 强调色**按饱和度取最大**,不按亮度。强调色在面板里只"染一点"(见 `theme::glass_colors`),
///   要的是最有颜色的那一块 —— 取最亮的话,一张雪景的强调色会是白色,染上去等于没染。
pub fn palette_from_rgba(rgba: &[u8], width: u32, height: u32) -> Vec<String> {
    let fallback = || {
        FALLBACK_PALETTE
            .iter()
            .map(|entry| (*entry).to_string())
            .collect::<Vec<_>>()
    };
    if width == 0 || height == 0 || rgba.len() < width as usize * height as usize * 4 {
        return fallback();
    }
    let blocks = block_averages(rgba, width, height);
    if blocks.is_empty() {
        return fallback();
    }

    let mut by_luma = blocks.clone();
    by_luma.sort_by(|left, right| {
        luma(*left)
            .partial_cmp(&luma(*right))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let quantile = |fraction: f64| -> Rgb {
        let last = by_luma.len() - 1;
        by_luma[((last as f64 * fraction).round() as usize).min(last)]
    };
    let accent = blocks
        .iter()
        .copied()
        .max_by(|left, right| {
            saturation(*left)
                .partial_cmp(&saturation(*right))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or_else(|| quantile(0.5));

    vec![
        hex(quantile(0.0)),
        hex(quantile(0.5)),
        hex(accent),
        hex(quantile(0.8)),
        hex(quantile(1.0)),
    ]
}

/// 一个 8 位 RGB 三元组。
type Rgb = (u8, u8, u8);

fn block_averages(rgba: &[u8], width: u32, height: u32) -> Vec<Rgb> {
    let mut blocks = Vec::with_capacity((GRID_COLS * GRID_ROWS) as usize);
    for row in 0..GRID_ROWS {
        let top = height * row / GRID_ROWS;
        let bottom = (height * (row + 1) / GRID_ROWS).max(top + 1).min(height);
        for col in 0..GRID_COLS {
            let left = width * col / GRID_COLS;
            let right = (width * (col + 1) / GRID_COLS).max(left + 1).min(width);
            blocks.push(block_average(rgba, width, left, right, top, bottom));
        }
    }
    blocks
}

fn block_average(rgba: &[u8], width: u32, left: u32, right: u32, top: u32, bottom: u32) -> Rgb {
    let mut sums = [0u64; 3];
    let mut count = 0u64;
    let mut y = top;
    while y < bottom {
        let mut x = left;
        while x < right {
            let offset = (y as usize * width as usize + x as usize) * 4;
            // 入口已经查过一次长度,这里再挡一道:块边界是按整数除算出来的,
            // 万一上层给的 `width` 和真实行宽不一致,越界读比"少算几块"严重得多。
            if offset + 2 < rgba.len() {
                sums[0] += rgba[offset] as u64;
                sums[1] += rgba[offset + 1] as u64;
                sums[2] += rgba[offset + 2] as u64;
                count += 1;
            }
            x += SAMPLE_STEP;
        }
        y += SAMPLE_STEP;
    }
    if count == 0 {
        return (0, 0, 0);
    }
    (
        (sums[0] / count) as u8,
        (sums[1] / count) as u8,
        (sums[2] / count) as u8,
    )
}

/// 感知亮度(Rec.709 权重),0~1。绿色权重最大是因为人眼对绿最敏感 ——
/// 用 `(r+g+b)/3` 会给出一套"数学上对、看起来偏暗"的排序。
fn luma((r, g, b): Rgb) -> f32 {
    (0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32) / 255.0
}

/// 饱和度(HSL 的定义),0~1。纯灰是 0,越接近纯色越接近 1。
fn saturation((r, g, b): Rgb) -> f32 {
    let max = r.max(g).max(b) as f32;
    let min = r.min(g).min(b) as f32;
    if max <= 0.0 {
        return 0.0;
    }
    (max - min) / max
}

fn hex((r, g, b): Rgb) -> String {
    format!("#{r:02X}{g:02X}{b:02X}")
}

fn clamp01(value: f32) -> Option<f32> {
    clamp_unit(value, 0.0, 1.0)
}

/// 夹到 `[min, max]`;`NaN` / 无穷返回 `None`,由调用方给出该字段自己的默认值。
fn clamp_unit(value: f32, min: f32, max: f32) -> Option<f32> {
    value.is_finite().then(|| value.clamp(min, max))
}

/// `#RGB` / `#RRGGBB` → 统一成 `#RRGGBB` 大写;非法返回 `None`。
fn normalize_hex(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_start_matches('#');
    let expanded = match trimmed.len() {
        3 => trimmed.chars().flat_map(|c| [c, c]).collect::<String>(),
        6 => trimmed.to_string(),
        _ => return None,
    };
    if !expanded.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("#{}", expanded.to_ascii_uppercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 位置清单本身的内容和长度。**顺序不能动**:提示词里就是照这个顺序念给模型的,
    /// 改了这里而没改提示词,"左上"就会被理解成别的位置(同序这条由 `mcwp_ai` 那条测试盯)。
    #[test]
    fn 位置清单是九宫格加全图() {
        assert_eq!(AREA_NAMES.len(), 10);
        assert_eq!(AREA_NAMES[0], "左上");
        assert_eq!(AREA_NAMES[4], "正中");
        assert_eq!(AREA_NAMES[9], "全图");
        // 九个格子 + 一个全图,顺序是从左上往右下读
        assert_eq!(AREA_NAMES[1], "上中");
        assert_eq!(AREA_NAMES[2], "右上");
        assert_eq!(AREA_NAMES[3], "左中");
        assert_eq!(AREA_NAMES[5], "右中");
        assert_eq!(AREA_NAMES[6], "左下");
        assert_eq!(AREA_NAMES[7], "下中");
        assert_eq!(AREA_NAMES[8], "右下");
    }

    fn solid(width: u32, height: u32, rgb: Rgb) -> Vec<u8> {
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            rgba.extend_from_slice(&[rgb.0, rgb.1, rgb.2, 255]);
        }
        rgba
    }

    /// 一张从上到下由黑到白的渐变图。
    fn gradient(width: u32, height: u32) -> Vec<u8> {
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            let value = (y * 255 / height.max(1)) as u8;
            for _ in 0..width {
                rgba.extend_from_slice(&[value, value, value, 255]);
            }
        }
        rgba
    }

    fn luma_of_hex(text: &str) -> f32 {
        let value = u32::from_str_radix(&text[1..], 16).unwrap();
        luma((
            ((value >> 16) & 0xFF) as u8,
            ((value >> 8) & 0xFF) as u8,
            (value & 0xFF) as u8,
        ))
    }

    #[test]
    fn 调色板从深到浅且是合法色值() {
        let palette = palette_from_rgba(&gradient(64, 64), 64, 64);
        assert_eq!(palette.len(), PALETTE_SLOTS);
        // 第 0 槽(底色)必须比第 4 槽(最亮)暗 —— 面板玻璃的明暗层次全靠这个顺序
        assert!(luma_of_hex(&palette[0]) < luma_of_hex(&palette[4]));
        assert!(luma_of_hex(&palette[3]) <= luma_of_hex(&palette[4]));
        for entry in &palette {
            assert_eq!(entry.len(), 7, "{entry} 应该是 #RRGGBB");
        }
    }

    #[test]
    fn 强调色挑最有颜色的那块而不是最亮的() {
        // 画面大部分纯白(最亮但没色),左下角一块正红(最有色)
        let mut rgba = solid(64, 64, (255, 255, 255));
        for y in 40..64 {
            for x in 0..20 {
                let offset = (y * 64 + x) * 4;
                rgba[offset..offset + 4].copy_from_slice(&[200, 20, 20, 255]);
            }
        }
        let palette = palette_from_rgba(&rgba, 64, 64);
        let value = u32::from_str_radix(&palette[2][1..], 16).unwrap();
        let (r, g, b) = ((value >> 16) & 0xFF, (value >> 8) & 0xFF, value & 0xFF);
        assert!(r > g && r > b, "强调色应该是那块红,实际是 {}", palette[2]);
    }

    #[test]
    fn 坏输入只报兜底色不越界读() {
        // 声明 64×64 却只给了一丁点字节:必须走兜底
        let short = palette_from_rgba(&[1, 2, 3, 4], 64, 64);
        assert_eq!(short.len(), PALETTE_SLOTS);
        assert_eq!(short[0], FALLBACK_PALETTE[0]);
        assert_eq!(palette_from_rgba(&[], 0, 0).len(), PALETTE_SLOTS);
    }

    #[test]
    fn 动效越界会被夹回来() {
        // 外层必须用 `r##`:里面全是 `"#RRGGBB"`,裸 `r#"…"#` 会被 `"#` 提前收掉。
        let photo = Photo::parse(
            r##"{
              "prompt": "雪山日出",
              "motion": { "kind": "snow", "intensity": 9.0, "zoom": -3.0, "pan": 8.0, "speed": 1.0 },
              "palette": ["#0B1020", "zzz"]
            }"##,
        )
        .unwrap();
        assert_eq!(photo.motion.kind, MotionKind::Snow);
        assert_eq!(photo.motion.intensity, 1.0);
        assert_eq!(photo.motion.zoom, 0.0);
        assert_eq!(photo.motion.pan, 1.0);
        assert_eq!(photo.motion.speed, MotionRecipe::SPEED_MAX);
        assert_eq!(photo.palette.len(), PALETTE_SLOTS);
        assert_eq!(photo.palette[0], "#0B1020");
        // 非法色值落到兜底槽位
        assert_eq!(photo.palette[1], FALLBACK_PALETTE[1]);
        // 标题没写就从提示词里来
        assert_eq!(photo.title, "雪山日出");
        // id 没写也得有一个能当文件名的
        assert_eq!(photo.id.len(), 16);
    }

    #[test]
    fn 未知动效名回退到默认而不是报错() {
        let photo = Photo::parse(r#"{ "prompt": "x", "motion": { "kind": "地震" } }"#).unwrap();
        assert_eq!(photo.motion.kind, MotionKind::Still);
    }

    #[test]
    fn id_由时间戳与提示词共同决定() {
        let a = Photo::new_id("雨夜城市", 1_760_000_000);
        let b = Photo::new_id("雨夜城市", 1_760_000_000);
        let c = Photo::new_id("雨夜城市", 1_760_000_001);
        let d = Photo::new_id("雪山日出", 1_760_000_000);
        assert_eq!(a, b, "同一秒同一提示词应该同名(可复现)");
        assert_ne!(a, c, "时间戳要参与");
        assert_ne!(a, d, "提示词要参与");
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
