//! MC 壁纸 · 对话 → 生图提示词 → 图片的模型客户端(OpenAI 兼容接口)。
//!
//! 一路上有**两次调用**,分属两家(可以是同一把 key、同一家服务商):
//! 1. [`Client::plan_photo`]:用户一句话 → 一句**给生图模型的提示词** + 一套动效配方。
//!    这一步是文本模型干的,正是它擅长、而生图模型完全不会的事(见 `PhotoDraft` 的说明)。
//! 2. [`Images::generate`]:那句提示词 → 一张图的字节。
//!
//! 上一代那条路(模型输出一份 `WallpaperSpec` JSON、本地着色器现算画面)已经随**预设清零**退休:
//! 那是"用文字填参数",而现在要的是"直接生成一张图"。留下这份说明,是因为下面好几处选择
//! (超时、`response_format`、`thinking`)是在那条路上踩出来的,换路之后依然成立。
//!
//! 几个刻意的选择:
//! - **第 1 步失败不致命**:`plan_photo()` 绝不返回 Err —— 模型答不上来就退化成"拿用户原话去画"
//!   (见 `fallback_image_prompt`)。用户敲了字,桌面总得有反应,不能弹个错误框了事。
//! - **只有第 2 步会失败**:生图接口真的挂了才会报错,那时由调用方决定怎么跟用户交代。
//! - **一句提示词就该是干净的**:模型偶尔把 `motion` 写成裸字符串而不是对象,解析时就地认下来
//!   (不重试 —— 实测把错误丢回去它会照抄原样,白等一轮)。
//! - **超时 45 s**:方案里那句"LLM 超时默认 3 s"是给 T1 环境联动用的;这里是用户敲完字等结果的
//!   T3/T2,3 s 太紧(中文长回复常态 5~15 s)。调用跑在后台线程 + 面板显示"生成中…",等得起。
//!   生图那一侧更慢(选图那一次要十几秒),所以 `Images` 用**180 s** 超时。
//! - **不传 `response_format: json_object`**:各家的兼容实现对这个字段态度不一(有的直接 400),
//!   提示词里已经写死"只输出 JSON",再加一道本地提取,比赌服务商实现稳。
//! - **`thinking` 只在配置里显式写了才发**:智谱 GLM-4.7 系列默认开深度思考,一句"换张壁纸"也要先
//!   想十几秒;可这个字段别家不认(理由同 `response_format`)。所以做成可选 —— `ai.json` 里写了
//!   `"thinking": false` 才带上 `{"type":"disabled"}`,不写就完全不出现在请求体里。
//! - **生图返回的 URL 当场下载**:智谱那些地址只有 7 天有效期,只存 URL 等于存了个会过期的壁纸。

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use mcwp_spec::{MotionElement, MotionKind, MotionPlan, MotionRecipe, Photo};
use serde::{Deserialize, Serialize};
use ureq::tls::{TlsConfig, TlsProvider};

/// 没配服务商时的默认值:指向**智谱**(2026-10-15 拍板先拿它的免费档试)。
/// 免费的那几家都是同一套 OpenAI 兼容接口,换个 `base_url` 就行 ——
/// 硅基流动 `https://api.siliconflow.cn/v1`、OpenRouter `https://openrouter.ai/api/v1`、
/// DeepSeek `https://api.deepseek.com`(按量计费)。
pub const DEFAULT_BASE_URL: &str = "https://open.bigmodel.cn/api/paas/v4";

/// 默认模型:智谱免费档里的 GLM-4-Flash(`glm-4-flash` 是它的别名,写全版本号更明确)。
/// 智谱另有个**也免费**的 `glm-4.7-flash`,更新更强,但它属于默认开深度思考的 4.7 系列 ——
/// 想换过去记得同时配 `"thinking": false`,不然一句话要等十几秒。
pub const DEFAULT_MODEL: &str = "glm-4-flash-250414";

/// 默认生图服务商。还是智谱、还是同一把 key —— 生图和对话共用一个接口域,
/// 所以我们不额外引入第二家(要换别家,把 `image_base_url` 填上就行)。
///
/// 免费档默认会在**右下角盖一枚"AI生成"角标**(约占宽 9% × 高 5%)。以前是靠构图那条
/// cover 裁切顺手把它裁掉的 —— 屏比图宽时上下各裁 5% 左右,**碰巧**盖住它;屏比图窄时
/// 就原样露在画面里(见 `mcwp-render/src/photo.wgsl` 的 `cover_uv`,它是居中等量裁,
/// 不是"专门把水印推出画面")。现在改成正路:请求里直接要求别画(见 `DEFAULT_IMAGE_WATERMARK`)。
/// 要完全绕开这家的水印也可以换一家(硅基流动的 Kolors / FLUX.1-schnell 也免费),所以地址这里是配置项。
pub const DEFAULT_IMAGE_BASE_URL: &str = DEFAULT_BASE_URL;

/// 默认生图模型:**`glm-image`**。
///
/// 2026-10-16 从 `cogview-3-flash`(免费档)换过来的,是用户点的名。实测的账:
///
/// | | `cogview-3-flash` | `glm-image` |
/// | --- | --- | --- |
/// | 一张要多久 | 9~16 s | **37~84 s**(同一句提示词、1472×1088 两次:37.4 s / 72.4 s) |
/// | 尺寸规则 | 16 的倍数 | **32 的倍数**(见 `SIZE_STEP`,换模型时尺寸规则跟着换了) |
/// | 总像素上限 | 2²¹ | **2²²** |
/// | 尺寸怎么定 | 按屏幕算 | 还是按屏幕算(**不留推荐档的余地**,见 `fit_size`) |
/// | `quality` | 认,而且实测有效 | 认(200),但**效果未验证** |
/// | `watermark_enabled` | 认 | 认(实测 false 那张右下角是干净的) |
///
/// 慢是真慢:`--images-selftest` 那种"等一张图"的体感从二十几秒变成一分钟出头,
/// 所以生图那条路的超时也跟着抬到了 5 分钟(见 `Images::new`)。
pub const DEFAULT_IMAGE_MODEL: &str = "glm-image";

/// 默认生图**画质**:`hd`。
///
/// 智谱这个接口有个 `quality` 参数,值是 `hd` / `standard`,**不写就是 standard**。
/// 实测(2026-10-16,同一条提示词、同样 1920×1088,只差这一个字段):
/// 高频细节(平均绝对拉普拉斯)从 20.9 抬到 **29.6(+41%)**,文件 413 KB → 440 KB。
/// 也就是说"生成出来的壁纸看着糊"有很大一部分是**我们没要高清**。
///
/// 值只认这两个(`4k` 直接 400 Bad Request),所以这里不做"越大越好"的自动升级。
pub const DEFAULT_IMAGE_QUALITY: &str = "hd";

/// 默认要不要服务商那枚「AI生成」角标:**不要**(`false`)。
///
/// 生图请求里有个 `watermark_enabled`,不写就是 `true`(显式角标 + 隐式数字水印)。
/// 官方给的关法是账号先签「去水印」免责声明(个人中心 → 安全管理 → 去水印管理),
/// 那侧放开之后请求里传 `false` 才生效;**没签的账号会被服务商拒**,所以这里不是
/// 一了百了的开关,`Images::generate` 里配了一条针对性的兜底:只在这种报错上
/// 退回带水印重发一次(见 `is_watermark_refusal`)。
///
/// 实测(2026-10-16,同一句提示词、同样 1024×1024):`true` 那次右下角有「AI生成」,
/// `false` 那次干干净净,两次都是 HTTP 200。
pub const DEFAULT_IMAGE_WATERMARK: bool = false;

/// 默认的「图生视频」模型。
///
/// `cogvideox-3` 实测(2026-10-16):提交 200、**约 61 s 出片**、1080×1080、5.13 s、31 fps、带水印,
/// mp4 828 KB(H.264)。另有一个 `cogvideox-flash` 的名字,但**当前一直回 429**
/// (错误码 1305「该模型当前访问量过大」),所以默认不选它 —— 想试就改配置。
///
/// 它产出的 mp4 **地址只有 24 小时有效**(生图那批是 7 天),所以和生图一样:
/// 拿到就当场下载,绝不只存地址。
pub const DEFAULT_VIDEO_MODEL: &str = "cogvideox-3";

/// 默认的**视觉模型**:动效分析那一步要"看"那张图(见 `plan_motion_vision`)。
///
/// 智谱的 `glm-4v-flash` 是**免费**档(官方定价页写的是"免费模型,支持图像理解"),
/// 地址和 key 直接继承对话那一套,所以不用再多填一个输入框。
///
/// 为什么非要"看":在拿到图之前,这一步的输入只有标题和提示词 —— 一个看不到画面的模型,
/// 只能写出"缓慢推近"这种万能句,而用户要的是"灯在闪、车在走、云在飘"这些**画面里才有的事**。
pub const DEFAULT_VISION_MODEL: &str = "glm-4v-flash";

/// 一次下载最多吃多少字节。
///
/// 为什么必须显式给:`ureq` 的 `read_to_vec()` **默认卡在 10 MB**,而 10 秒 / 1080p 的成片
/// 正好会越过它 —— 2026-10-16 实测那一条「都市夜景黄昏」(1718×1080、10 秒、灯光闪烁 + 云流动,
/// 画面细节多、码率高)就撞在这上面:`读视频字节失败 / the response body is larger than
/// request limit: 10485760`。**这条视频当时已经生成完了、钱也花了**,死在最后一步"取回来"上,
/// 而且报错看着像网络问题,一点也看不出是本地那道闸。
///
/// 取 128 MB:10 秒的片子按 2 MB/s 算也就 20 MB,给到 128 MB 是留给"以后放更长 / 更高码率"的余量;
/// 再大就不该全塞进内存了(那时候要改成边下边落盘)。
const MAX_DOWNLOAD_BYTES: u64 = 128 << 20;

/// 轮询那条异步接口的间隔。它一次要跑几十秒,查得太密没有意义,太疏又会让"好了"晚到几秒。
const VIDEO_POLL_INTERVAL: Duration = Duration::from_secs(8);

/// 一条视频最多等多久。
///
/// 实测差得极远:闲的时候 61 s 就出片,2026-10-16 那天同样一条 10 秒片**等了 7 分钟**
/// 还在 PROCESSING(`--video-fetch` 亲自查到过);2026-10-17 的 `quality` 档甚至跑了
/// **16 分 49 秒**才 SUCCESS。所以给到 30 分钟 —— 到点还是报错,不让面板上那颗
/// 「做成动态壁纸」永远转下去;真超了也别慌,任务号会跟着报错一起打出来,
/// `--video-fetch <任务号> <库 id>` 能把它捡回来。
const VIDEO_WAIT_TIMEOUT: Duration = Duration::from_secs(1800);

/// 动效提示词最长多少字。
///
/// 旧值 200 是按"一句短清单"定的;现在这条路改成**英文长描述**(官方 CogVideoX
/// 就是在长提示词下训练的),200 字会把一段 60 词的英文直接砍半。接口对 `prompt`
/// 的硬上限是 512 字符,所以这里和 `MAX_VIDEO_PROMPT_CHARS` 对齐。
const MAX_MOTION_PROMPT_CHARS: usize = 512;

/// 所有动效提示词共用的收尾。
///
/// 不能只写 `seamless`:那只是"看起来不跳",视频模型完全可能理解成"整段几乎不动"。
/// 这里把动作幅度和位置钉死 —— 中段最大、末帧回到起始姿态;再把曝光、色彩和光照锁住,
/// 避免模型用"太阳落山 / 整体明暗变化"来伪造运动。
const MOTION_LOOP_TAIL: &str = "Each motion completes one full visible cycle and returns to its starting pose; no camera movement, zoom, pan or orbit, and exposure and color stay fixed.";

/// 提交被"服务商在忙"挡回来时的退避(秒)。
///
/// 2026-10-17 实测,忙的时候提交直接回 `429`:
/// `{"error":{"code":"1305","message":"该模型当前访问量过大,请您稍后再试"}}`。
/// **收到 429 说明这一单根本没被受理、也没计费**,所以等一等再交是安全的;
/// 而"接下了、跑十分钟再 FAIL"那种**绝对不能重提**(钱已经花了),见 `wait_and_download`。
const SUBMIT_BUSY_BACKOFF: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(45),
];

/// 图生视频接口对 `prompt` 的硬上限:512 个字符(智谱 `videos/generations` 的文档写的)。
///
/// 和上面那个 200 是**两回事**:200 是"我们想要的克制"(一句运镜就够),
/// 512 是"服务商允许的极限"。元素级方案会把几件事并排写进去,所以按 512 收口;
/// 真超了就是 `truncate` 兜底 —— 宁可少一个元素,不能让整条请求被拒。
const MAX_VIDEO_PROMPT_CHARS: usize = 512;

/// 生图接口对尺寸的硬要求。**这是接口自己发的 4xx 正文抄下来的**(2026-10-16,`glm-image`):
///
/// > size的长宽均需满足512px-2880px之间,且为32整数倍,并保证最大像素数不超过2^22px
///
/// 三条都可能踩:`1920×1080` 会被**直接拒绝**(1080 不是 32 的倍数)。
/// **更坑的是换模型会换规则**:CogView 那阵子是"16 的倍数、2²¹",库里那批
/// 1600×896 / 1808×1136 到 `glm-image` 这边**全不合法**(1808/32 = 56.5)。
/// 所以尺寸还是得按屏幕算一个,只是这回要**自己**把这四条压住(见 `fit_size`:
/// 对齐 32 的倍数 → 长边不超 2880 → 总像素不超 2²² → 短边不低于 512,全程等比)。
const SIZE_MIN: u32 = 512;
const SIZE_MAX: u32 = 2880;
const SIZE_STEP: u32 = 32;
const SIZE_PIXEL_MAX: u64 = 1 << 22;

/// 提示词最长多少字。太长的提示词对 `cogview` 那种小模型是负担,反而容易画糊。
const MAX_IMAGE_PROMPT_CHARS: usize = 400;

/// 请求超时(见文件头说明)。
const TIMEOUT: Duration = Duration::from_secs(45);

/// 模型回复里能塞给用户看的最大字符数,免得某家模型话痨把面板撑爆。
const MAX_REPLY_CHARS: usize = 120;

/// 生图这条路的第一跳:用户一句话 → 一句**给文生图模型的提示词** + 一套动效配方。
///
/// 为什么不把用户原话直接丢给生图模型 —— 那正是这个结构存在的理由:
/// - **两边的语言不一样**。用户说的是"我想要个安静点的桌面",生图模型要听的是
///   "清晨薄雾的湖面,远景雪山,柔和的冷蓝调,开阔构图,高清数字插画"。
///   这中间的翻译,正是文本模型擅长的、而生图模型完全不会的那一步。
/// - **动效是本地的事,生图模型不知道**。雪景该配落雪、湖面该配水波 —— 这个判断需要
///   先理解需求,再"看"一眼自己写的画面描述,只能由写提示词的那一方顺手给出。
/// - 这条路上**多花的那一次调用是值的**:它换来的是一张构图稳定的图,而不是一张
///   靠运气、还得反复重生的图(生图一次 9~16 s,重试成本比一次文本调用高得多)。
pub struct PhotoDraft {
    /// 说给用户听的一句话
    pub reply: String,
    /// 这张壁纸的短标题
    pub title: String,
    /// 真正发给生图模型的提示词
    pub image_prompt: String,
    /// 出图之后怎么动
    pub motion: MotionRecipe,
    /// 走了本地降级(模型没答上来,提示词是拿用户原话拼的)
    pub degraded: bool,
    /// 实际发出去的请求次数
    pub calls: u32,
}

/// 一份**能直接用**的配置(缺哪个字段都已经填上默认值),`Client::new` 收的就是它。
///
/// 优先级:**环境变量 > `%APPDATA%\MC壁纸\ai.json` > 内置默认**。
///
/// 环境变量那几个(`MCWP_AI_KEY` / `MCWP_AI_BASE_URL` / `MCWP_AI_MODEL` / `MCWP_AI_THINKING`)
/// 是给"临时试一家"和自检用的;日常走面板的设置页,写进配置文件 —— 免得把 key 塞进快捷方式。
#[derive(Debug, Clone)]
pub struct Config {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    /// 「深度思考」开关。`None` = 请求体里不带这个字段(对不认它的服务商最安全)。
    pub thinking: Option<bool>,
    /// 看图的模型。和对话**共用地址与 key**,只换模型名(见 `DEFAULT_VISION_MODEL`)。
    pub vision_model: String,
}

/// 落盘的那份配置,也就是 `%APPDATA%\MC壁纸\ai.json` 的内容。
///
/// 单独做成公开类型,是因为**面板的设置页要拿它回填输入框** —— 那时候多半还没配 key,
/// `Config::load()` 会直接报错,回填不了。读写共用这一份字段定义,免得两边漂移。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Stored {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub api_key: String,
    /// 见文件头:只在设置页里显式勾了才写出来(`None` 连字段都不落),免得把别家 400 掉。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
    /// 生图单独一套配置,留空就**跟对话那套共用**(智谱一把 key 管两个接口)。
    /// 做成独立字段是为了"对话用 A 家、生图用 B 家"这种真实需求 —— 免费额度各花各的。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub image_base_url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub image_model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub image_api_key: String,
    /// 生图画质:`hd` / `standard`。**留空是合法的**:空 = 用内置默认(`hd`,见
    /// `DEFAULT_IMAGE_QUALITY`),免得老配置文件升级上来反而变糊。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub image_quality: String,
    /// 生图带不带「AI生成」角标。**留空是合法的**:空 = 用内置默认(`false`,见
    /// `DEFAULT_IMAGE_WATERMARK`)。写 `Option<bool>` 而不是 `bool`,是为了区分
    /// "用户明确要带水印"和"还没设过" —— 前者才落盘,后者连字段都不出现。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_watermark: Option<bool>,
    /// 「图生视频」那一套。它和生图**基本总是同一家**(同一把 key 的同一个接口域下),
    /// 所以这里只留一个模型名:地址和 key 都从生图 / 对话那两组继承下来(见 `VideosConfig::load`)。
    /// 真要用另一家,再补字段不迟 —— 现在多两个输入框只会让人不知道该填哪个。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub video_model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub video_base_url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub video_api_key: String,
    /// 视觉模型(动效分析那一步看图用)。留空就跟对话那套共用地址和 key。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub vision_model: String,
}

impl Config {
    pub fn load() -> Result<Self> {
        let raw = read_stored().unwrap_or_default();

        let env = |key: &str| {
            std::env::var(key)
                .ok()
                .filter(|value| !value.trim().is_empty())
        };
        let base_url = env("MCWP_AI_BASE_URL")
            .or_else(|| non_empty(&raw.base_url))
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let model = env("MCWP_AI_MODEL")
            .or_else(|| non_empty(&raw.model))
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let vision_model = env("MCWP_VISION_MODEL")
            .or_else(|| non_empty(&raw.vision_model))
            .unwrap_or_else(|| DEFAULT_VISION_MODEL.to_string());
        let Some(api_key) = env("MCWP_AI_KEY").or_else(|| non_empty(&raw.api_key)) else {
            return Err(anyhow!("{}", Self::missing_key_hint()));
        };
        // 环境变量写了就认它:`1/true/on/yes` 算开,写别的都算关;没写就跟着 ai.json。
        let thinking = env("MCWP_AI_THINKING")
            .map(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "on" | "yes"
                )
            })
            .or(raw.thinking);
        Ok(Self {
            base_url,
            model,
            api_key,
            thinking,
            vision_model,
        })
    }

    /// 给设置页回填:**文件里现在写着什么**,空着的字段用内置默认补上。
    ///
    /// 特意不合并环境变量 —— 界面要显示的是"这份文件的内容",改了才写得回去;
    /// 环境变量盖没盖住由 `env_override()` 单独告诉界面。
    pub fn stored() -> Stored {
        let mut file = read_stored().unwrap_or_default();
        if file.base_url.trim().is_empty() {
            file.base_url = DEFAULT_BASE_URL.to_string();
        }
        if file.model.trim().is_empty() {
            file.model = DEFAULT_MODEL.to_string();
        }
        if file.image_base_url.trim().is_empty() {
            file.image_base_url = DEFAULT_IMAGE_BASE_URL.to_string();
        }
        if file.image_model.trim().is_empty() {
            file.image_model = DEFAULT_IMAGE_MODEL.to_string();
        }
        if file.image_quality.trim().is_empty() {
            file.image_quality = DEFAULT_IMAGE_QUALITY.to_string();
        }
        if file.vision_model.trim().is_empty() {
            file.vision_model = DEFAULT_VISION_MODEL.to_string();
        }
        file
    }

    /// 把设置页填的东西写回 `ai.json`。
    ///
    /// 落盘用**临时文件 + 改名**(和会话那份同理,见 `sessions.rs`):直接往 `ai.json` 上盖,
    /// 写到一半掉电就只剩半截 JSON,而这份文件里存着 key,坏掉最难查。写出来是带缩进的格式,
    /// 用户自己打开看 / 手改都方便。
    pub fn save(stored: &Stored) -> Result<()> {
        let path = config_file()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("建目录失败:{}", dir.display()))?;
        }
        let text = serde_json::to_string_pretty(stored).context("序列化模型配置失败")?;
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, text).with_context(|| format!("写失败:{}", temp.display()))?;
        std::fs::rename(&temp, &path).with_context(|| format!("替换失败:{}", path.display()))?;
        Ok(())
    }

    /// 有环境变量在盖配置文件的字段吗?面板据此提醒一句:"你在这儿改的不会生效"。
    pub fn env_override() -> bool {
        [
            "MCWP_AI_KEY",
            "MCWP_AI_BASE_URL",
            "MCWP_AI_MODEL",
            "MCWP_AI_THINKING",
            "MCWP_IMAGE_KEY",
            "MCWP_IMAGE_BASE_URL",
            "MCWP_IMAGE_MODEL",
            "MCWP_VISION_MODEL",
        ]
        .iter()
        .any(|key| std::env::var(key).is_ok_and(|value| !value.trim().is_empty()))
    }

    /// 没配 key 时给人话,别只丢一句"缺少 api_key"。
    fn missing_key_hint() -> String {
        let path = config_file()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "%APPDATA%\\MC壁纸\\ai.json".to_string());
        format!(
            "还没配模型。写一份 {path}:\n\
             {{\"base_url\": \"{DEFAULT_BASE_URL}\", \"model\": \"{DEFAULT_MODEL}\", \"api_key\": \"你的key\"}}\n\
             免费可用的有几家(都是同一套 OpenAI 兼容接口,换 base_url / model 就行):\n\
             · 智谱 GLM-4-Flash —— 免费、国内直连,内置默认:{DEFAULT_BASE_URL} + {DEFAULT_MODEL}\n\
             · 智谱 GLM-4.7-Flash —— 也免费、更新更强:model 换 glm-4.7-flash,再加 \"thinking\": false\n\
             · 硅基流动 —— 注册送额度:https://api.siliconflow.cn/v1\n\
             · OpenRouter / Groq —— 免费档,需要能连外网\n\
             · DeepSeek —— 按量计费(便宜):https://api.deepseek.com + deepseek-chat\n\
             临时试一家也可以只设环境变量 MCWP_AI_KEY / MCWP_AI_BASE_URL / MCWP_AI_MODEL / MCWP_AI_THINKING。"
        )
    }
}

/// 生图那条路缺 key 时给人话。
///
/// 单独写一条而不是复用 `missing_key_hint`,是因为**这里多数时候不是"没配",而是配错了地方**:
/// 用户看着对话能聊天、以为配置好了,结果是 `api_key` 空白(靠 `MCWP_AI_KEY` 环境变量跑着)。
fn missing_image_key_hint() -> String {
    let path = config_file()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "%APPDATA%\\MC壁纸\\ai.json".to_string());
    format!(
        "还没配生图。在面板「设置」里填密钥即可,或者写进 {path}:\n\
         {{\"base_url\": \"{DEFAULT_BASE_URL}\", \"model\": \"{DEFAULT_MODEL}\", \
         \"api_key\": \"你的key\", \"image_model\": \"{DEFAULT_IMAGE_MODEL}\", \
         \"image_quality\": \"{DEFAULT_IMAGE_QUALITY}\"}}\n\
         生图默认**跟对话共用同一把 key**(智谱一把 key 管两个接口),想分开就再填 \
         image_base_url / image_model / image_api_key。\n\
         image_quality 只认 hd(默认,细节多)和 standard(服务商默认,明显更糊)。\n\
         image_watermark 是\"要不要服务商那枚「AI生成」角标\",默认 false(不带);\
         关得掉的前提是账号先在「个人中心 → 安全管理 → 去水印管理」签过免责声明。\n\
         临时试一家也可以只设环境变量 MCWP_IMAGE_KEY / MCWP_IMAGE_BASE_URL / MCWP_IMAGE_MODEL / \
         MCWP_IMAGE_QUALITY / MCWP_IMAGE_WATERMARK。"
    )
}

/// 生图那一套配置。缺哪个字段都已经填上默认值。
///
/// 优先级和对话那边一样:**环境变量 > `ai.json` 的 image_* 字段 > `ai.json` 的对话字段 > 内置默认**。
/// 中间那两层是刻意的 —— 只配了对话、没碰生图的话,生图自动跟着走,不用把同一个 key 填两遍。
#[derive(Debug, Clone)]
pub struct ImagesConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    /// 画质:`hd` / `standard`。别的值服务商会 400,所以这里不做"猜你想说什么"。
    pub quality: String,
    /// 带不带「AI生成」角标。见 `DEFAULT_IMAGE_WATERMARK` —— 关掉它需要账号先签名。
    pub watermark: bool,
}

impl ImagesConfig {
    pub fn load() -> Result<Self> {
        let raw = read_stored().unwrap_or_default();
        let env = |key: &str| {
            std::env::var(key)
                .ok()
                .filter(|value| !value.trim().is_empty())
        };
        let base_url = env("MCWP_IMAGE_BASE_URL")
            .or_else(|| non_empty(&raw.image_base_url))
            .or_else(|| non_empty(&raw.base_url))
            .unwrap_or_else(|| DEFAULT_IMAGE_BASE_URL.to_string());
        let model = env("MCWP_IMAGE_MODEL")
            .or_else(|| non_empty(&raw.image_model))
            .unwrap_or_else(|| DEFAULT_IMAGE_MODEL.to_string());
        let quality = env("MCWP_IMAGE_QUALITY")
            .or_else(|| non_empty(&raw.image_quality))
            .unwrap_or_else(|| DEFAULT_IMAGE_QUALITY.to_string());
        // 认不出的写法**继续往下走**(`and_then` 会把 `None` 留在原地),最后落到内置默认 ——
        // 见 `parse_bool`:用户写了个没见过的词时,拿它当 `false` 或者当 `true` 都是替人做决定。
        let watermark = env("MCWP_IMAGE_WATERMARK")
            .and_then(|value| parse_bool(&value))
            .or(raw.image_watermark)
            .unwrap_or(DEFAULT_IMAGE_WATERMARK);
        let Some(api_key) = env("MCWP_IMAGE_KEY")
            .or_else(|| non_empty(&raw.image_api_key))
            .or_else(|| env("MCWP_AI_KEY"))
            .or_else(|| non_empty(&raw.api_key))
        else {
            return Err(anyhow!("{}", missing_image_key_hint()));
        };
        Ok(Self {
            base_url,
            model,
            api_key,
            quality,
            watermark,
        })
    }
}

/// 生图接口的客户端。
///
/// 和 `Client`(对话)分开,而不是给 `Client` 加一个方法,有三个实打实的理由:
/// - **超时差一个数量级**。对话 45 s 已经算宽裕,生图 9~16 s 是常态、排队时能到一分钟,
///   再叠加一次"下载图片",用同一个 agent 要么把对话拖成龟速、要么把生图掐在半路。
/// - **地址不同**。对话打 `/chat/completions`,生图打 `/images/generations`,
///   配置也各自独立(见 `ImagesConfig`)。
/// - 这两件事**失败的意义也不同**:对话失败降级成"我记下了",生图失败是"没画出图",
///   面板要给的提示完全不一样,混在一起只会让两边的错误处理都变糊。
#[derive(Clone)]
pub struct Images {
    cfg: ImagesConfig,
    agent: ureq::Agent,
}

/// 一次生图的产物。
///
/// 同时带 URL 和字节,是因为**两个都要用**:字节拿去解码 / 当纹理,URL 留给"图生视频"
/// (那个接口要的是 `image_url` 而不是上传文件,见方案里的 L2)。
pub struct GeneratedImage {
    pub url: String,
    pub bytes: Vec<u8>,
    /// 这张图**实际**带不带那枚「AI生成」角标。
    ///
    /// 它不是"配置里怎么写的",而是"这一张是怎么出来的":配置要的是不带,而账号没签
    /// 去水印声明时兜底重发过一次 —— 那种情况下调用方得能告诉用户"这张是带水印的",
    /// 不然用户只会看到一个角标,不知道是哪儿来的(见 `generate`)。
    pub watermarked: bool,
}

/// 一次生图请求的原始回执:状态码 + 响应体文本。
struct ImageReply {
    status: u16,
    body: String,
}

/// 这次报错说的是不是「你还不许去掉水印」。
///
/// **只在报错正文提到水印时才算**。理由:兜底重发是"用另一组参数再发一次",判断放宽了
/// 就会把别的错误(参数不合法、欠费、模型下线)也重发一遍 —— 轻则白等一轮,重则在
/// 按张计费的服务商那边多花一次钱。认不出来就只是不重发,报错原样往上抛。
fn is_watermark_refusal(status: u16, body: &str) -> bool {
    (400..500).contains(&status)
        && (body.contains("watermark") || body.contains("水印"))
}

impl Images {
    pub fn new(cfg: ImagesConfig) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            // 生图的等待时间比对话长得多,而且**换模型之后又长了一截**:
            // `cogview-3-flash` 实测 9~16 s,`glm-image` 同一句提示词两次是 37.4 s / 72.4 s
            // (尺寸越大越慢,2048² 那次 120 s 都没回来)。给到 5 分钟 —— 这一条超时的代价
            // 不是"重试一下",是**那一张的钱已经花了、图却拿不到**,所以宁可等。
            .timeout_global(Some(Duration::from_secs(300)))
            .http_status_as_error(false)
            // 理由同 `Client::new`:不显式挑 native-tls,ureq 会在 https 上直接 panic
            .tls_config(
                TlsConfig::builder()
                    .provider(TlsProvider::NativeTls)
                    .build(),
            )
            .build()
            .into();
        Self { cfg, agent }
    }

    /// 给人看的服务商 / 模型标识,写进日志和状态行。
    pub fn describe(&self) -> String {
        let host = self
            .cfg
            .base_url
            .split("//")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .unwrap_or(&self.cfg.base_url);
        // 画质和水印都写进来:这行是 `--images-selftest` 唯一能自己交代"按哪一档、带不带角标
        // 出图"的地方,而"为什么这么糊""怎么有个 AI 生成"这类问题,答案十有八九就在这几个字上
        // (见 §11.24 / §11.25)。
        format!(
            "{host} · {} · {} · {}",
            self.cfg.model,
            self.cfg.quality,
            if self.cfg.watermark { "带水印" } else { "无水印" }
        )
    }

    /// `base_url` 后面补 `/images/generations`。写成完整 endpoint 也认。
    fn endpoint(&self) -> String {
        let base = self.cfg.base_url.trim_end_matches('/');
        if base.ends_with("/images/generations") {
            base.to_string()
        } else {
            format!("{base}/images/generations")
        }
    }

    /// 出一张图:提交生成任务 → 拿到图片 URL → **当场把图下下来**。
    ///
    /// 为什么要立刻下载而不是把 URL 存起来慢慢取:智谱返回的图片 URL **7 天就失效**,
    /// 而壁纸是要长期留着的。存 URL = 存一个七天后的死链。
    /// `screen_width` / `screen_height` 是**屏幕分辨率**而不是"请求尺寸":要多大由 `fit_size`
    /// 按接口那三条规则折出来(见那儿),调用方只管把屏报上来。
    pub fn generate(
        &self,
        prompt: &str,
        screen_width: u32,
        screen_height: u32,
    ) -> Result<GeneratedImage> {
        let (width, height) = fit_size(screen_width, screen_height);
        let size = format!("{width}x{height}");
        // 按配置来;被拒了再退回带水印的(见 `is_watermark_refusal`)。
        // `watermark` 这个局部量跟着**实际发出去的那个值**走,所以最后那行日志说的是真话。
        let mut watermark = self.cfg.watermark;
        let mut reply = self.post_image(prompt, &size, watermark)?;
        if !watermark && is_watermark_refusal(reply.status, &reply.body) {
            tracing::warn!(
                "生图接口拒了「不带水印」这个要求,多半是这个账号还没签「去水印」免责声明\
                 (个人中心 → 安全管理 → 去水印管理);这一张退回带水印重发"
            );
            watermark = true;
            reply = self.post_image(prompt, &size, watermark)?;
        }
        if !(200..300).contains(&reply.status) {
            return Err(anyhow!(
                "生图接口返回 {}:{}",
                reply.status,
                truncate(&reply.body, 300)
            ));
        }
        let raw = reply.body;
        let parsed: ImageResponse = serde_json::from_str(&raw)
            .with_context(|| format!("生图响应没有 data[].url:{}", truncate(&raw, 200)))?;
        let url = parsed
            .data
            .into_iter()
            .map(|entry| entry.url)
            .find(|url| !url.trim().is_empty())
            .ok_or_else(|| anyhow!("生图接口说成功了,但没给图片地址"))?;
        let bytes = self.download(&url)?;
        tracing::info!(
            "生图完成:{}×{} · {} · {} · {} KB · {}",
            width,
            height,
            self.cfg.quality,
            if watermark { "带水印" } else { "无水印" },
            bytes.len() / 1024,
            truncate(prompt, 60)
        );
        Ok(GeneratedImage {
            url,
            bytes,
            watermarked: watermark,
        })
    }

    /// 发一次生图请求,把状态码和响应体原样拿回来。
    ///
    /// 拆出来是为了那条兜底重发:同一句提示词、同一个尺寸要发两次,而"发 + 读回 body"
    /// 这段没有理由写两遍(写两遍就会有一遍漏掉超时/鉴权头的改动)。
    fn post_image(&self, prompt: &str, size: &str, watermark: bool) -> Result<ImageReply> {
        let body = serde_json::to_string(&ImageRequest {
            model: self.cfg.model.as_str(),
            prompt,
            size: size.to_string(),
            quality: self.cfg.quality.as_str(),
            watermark_enabled: watermark,
        })?;
        let mut response = self
            .agent
            .post(&self.endpoint())
            .header("Content-Type", "application/json")
            .header("Authorization", &format!("Bearer {}", self.cfg.api_key))
            .send(body.as_str())
            .map_err(|err| anyhow!("请求生图失败:{err}"))?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .context("读生图响应失败")?;
        Ok(ImageReply { status, body })
    }

    /// 把图片 URL 抓成字节。走同一个 agent(超时和 TLS 配置都现成)。
    fn download(&self, url: &str) -> Result<Vec<u8>> {
        let mut response = self
            .agent
            .get(url)
            .call()
            .map_err(|err| anyhow!("下载图片失败:{err}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow!("下载图片返回 {}:{}", status.as_u16(), url));
        }
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_DOWNLOAD_BYTES)
            .read_to_vec()
            .context("读图片字节失败")?;
        if bytes.is_empty() {
            return Err(anyhow!("下载到的图片是空的:{}", truncate(url, 120)));
        }
        Ok(bytes)
    }
}

/// 「图生视频」这一套配置。
///
/// 优先级:环境变量 > `ai.json` 的 `video_*` > `ai.json` 的 `image_*` > `ai.json` 的对话字段 > 内置默认。
/// 中间那两层是**故意的**:图生视频和生图在智谱是同一个接口域、同一把 key,
/// 已经填过生图那组的人不该再填一遍。
#[derive(Debug, Clone)]
pub struct VideosConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

impl VideosConfig {
    pub fn load() -> Result<Self> {
        let raw = read_stored().unwrap_or_default();
        let env = |key: &str| {
            std::env::var(key)
                .ok()
                .filter(|value| !value.trim().is_empty())
        };
        let base_url = env("MCWP_VIDEO_BASE_URL")
            .or_else(|| non_empty(&raw.video_base_url))
            .or_else(|| non_empty(&raw.image_base_url))
            .or_else(|| non_empty(&raw.base_url))
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let model = env("MCWP_VIDEO_MODEL")
            .or_else(|| non_empty(&raw.video_model))
            .unwrap_or_else(|| DEFAULT_VIDEO_MODEL.to_string());
        let Some(api_key) = env("MCWP_VIDEO_KEY")
            .or_else(|| non_empty(&raw.video_api_key))
            .or_else(|| non_empty(&raw.image_api_key))
            .or_else(|| env("MCWP_AI_KEY"))
            .or_else(|| non_empty(&raw.api_key))
        else {
            return Err(anyhow!("{}", missing_image_key_hint()));
        };
        Ok(Self {
            base_url,
            model,
            api_key,
        })
    }
}

/// 一条视频生成到哪一步了。
#[derive(Debug, Clone, PartialEq)]
pub enum VideoProgress {
    /// 还在跑(服务商那边)
    Running,
    /// 出来了:视频地址 + 封面图地址(封面是它的第一帧,存下来当缩略图)
    Done { video_url: String, cover_url: String },
    /// 服务商说这条失败了
    Failed(String),
}

/// 一次图生视频的产物。和 `GeneratedImage` 同一套路:地址留着(日志 / 排查用),
/// 字节才是真正要落盘的东西 —— 地址 24 小时后就没了。
pub struct GeneratedVideo {
    pub url: String,
    pub bytes: Vec<u8>,
}

/// 「图生视频」接口的客户端。
///
/// 和 `Images` 分开建的理由,与生图和对话分开的理由**不完全一样**,值得说清楚:
/// - **它是两段式**:提交拿任务号 → 轮询异步结果。轮询那条路有自己的节奏(见 `VIDEO_POLL_INTERVAL`),
///   硬塞进"一次请求一次结果"的 `Images` 里,超时和重试都无从谈起。
/// - **它慢一个数量级**:生图十几秒,这个一分钟起步;超时给到 6 分钟,不能和生图共用一个。
/// - **它的请求体可能很大**:图片是用 base64 塞进 `image_url` 的(见 `image_data_url`),
///   那张图有几兆,请求体就有几兆 —— 这也是"别和别的调用共用一个 agent"的理由之一。
///
/// 一个**反直觉的坑**:轮询那条路会返回 HTTP 400,而 body 里其实是一条完全正常的任务状态
/// (实测踩到过 `{"task_status":"SUCCESS",...}` 配 400)。所以 `poll` 判成功**看 body 里的
/// `task_status`,不看 HTTP 码** —— 只看 HTTP 码会把已经出片的任务当成失败扔掉。
#[derive(Clone)]
pub struct Videos {
    cfg: VideosConfig,
    agent: ureq::Agent,
}

impl Videos {
    pub fn new(cfg: VideosConfig) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            // 一次请求最多等 6 分钟(轮询那条每次都是小请求,真正会慢的是下载视频),
            // 和"整条路最多等多久"是两回事,后者在 `animate` 里管
            .timeout_global(Some(Duration::from_secs(600)))
            .http_status_as_error(false)
            // 理由同 `Client::new`:不显式挑 native-tls,ureq 会在 https 上直接 panic
            .tls_config(
                TlsConfig::builder()
                    .provider(TlsProvider::NativeTls)
                    .build(),
            )
            .build()
            .into();
        Self { cfg, agent }
    }

    /// 给人看的服务商 / 模型标识,写进日志和状态行。
    pub fn describe(&self) -> String {
        let host = self
            .cfg
            .base_url
            .split("//")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .unwrap_or(&self.cfg.base_url);
        // 画质写进来:这行是 `--images-selftest` 唯一能自己说清楚"按哪一档出图"的地方,
        // 而"为什么这么糊"那个问题里,答案十有八九就在这几个字上(见 §11.24)。
        format!("{host} · {}", self.cfg.model)
    }

    /// 配置里那条地址的**根部**。写成完整 endpoint 也认:那样就把子路径摘掉,
    /// 因为提交和轮询两条路挂的是同一个根(见 `submit` / `poll`)。
    fn root(&self) -> String {
        let base = self.cfg.base_url.trim_end_matches('/').to_string();
        base.strip_suffix("/videos/generations")
            .map(|trimmed| trimmed.to_string())
            .unwrap_or(base)
    }

    /// 提交一条图生视频任务,拿回任务号。
    pub fn submit(&self, image_url: &str, prompt: &str, screen: (u32, u32)) -> Result<String> {
        self.submit_image(VideoImage::One(image_url), prompt, screen)
    }

    /// 提交一条**首尾帧**任务:首帧和尾帧同一张图(见 `VideoImage`)。
    ///
    /// **2026-10-17 起壁纸不再走这条。** 首尾都给同一张图时,模型只要"保持画面"就能
    /// 同时满足首帧和尾帧,实测运动被压成近乎静帧(多轮对照:首尾同图 0.06,
    /// 单图 quality 2.92 / speed 8.79,见 §11.31)。
    /// 保留它是为了接口兼容和诊断,不要拿它做壁纸。
    pub fn submit_looped(
        &self,
        image_url: &str,
        prompt: &str,
        screen: (u32, u32),
    ) -> Result<String> {
        self.submit_image(VideoImage::Pair([image_url, image_url]), prompt, screen)
    }

    fn submit_image(
        &self,
        image_url: VideoImage<'_>,
        prompt: &str,
        screen: (u32, u32),
    ) -> Result<String> {
        let size = size_field(video_size_for(screen));
        let url = format!("{}/videos/generations", self.root());
        // 忙的时候(429)退避重提:这一单没被受理、不计费,所以重提**不亏钱**;
        // 换成"接下了再 FAIL"那种就不能这么干(见 `SUBMIT_BUSY_BACKOFF`)。
        let mut busy = 0;
        // 无水印是用户已经签过免责声明的路径;万一账号没开通,服务商会 4xx 拒绝。
        // 和生图那条一样退回带水印重提一次,而不是让整个按钮失败。
        let mut watermark = false;
        loop {
            let body = serde_json::to_string(&VideoRequest {
                model: self.cfg.model.as_str(),
                prompt,
                image_url,
                quality: VIDEO_QUALITY,
                watermark_enabled: watermark,
                size: size.as_str(),
                duration: VIDEO_DURATION_SECS,
                with_audio: false,
            })?;
            let mut response = self
                .agent
                .post(&url)
                .header("Content-Type", "application/json")
                .header("Authorization", &format!("Bearer {}", self.cfg.api_key))
                .send(body.as_str())
                .map_err(|err| anyhow!("提交图生视频失败:{err}"))?;
            let status = response.status();
            let raw = response
                .body_mut()
                .read_to_string()
                .context("读图生视频响应失败")?;
            if !status.is_success() {
                if status.as_u16() == 429 {
                    if let Some(wait) = SUBMIT_BUSY_BACKOFF.get(busy) {
                        busy += 1;
                        tracing::warn!(
                            "服务商在忙(429:{}) —— {} 秒后重提第 {busy} 次(这单没受理、不计费)",
                            truncate(&raw, 120),
                            wait.as_secs()
                        );
                        std::thread::sleep(*wait);
                        continue;
                    }
                    return Err(anyhow!(
                        "服务商现在太忙,连提了 {} 次都被挡回来(429):{}。过一会儿再点就行 —— \
                         这一单没被受理,不花钱",
                        busy + 1,
                        truncate(&raw, 160)
                    ));
                }
                if !watermark && is_watermark_refusal(status.as_u16(), &raw) {
                    tracing::warn!(
                        "视频去水印被拒({}:{}) —— 退回带水印重提一次",
                        status.as_u16(),
                        truncate(&raw, 120)
                    );
                    watermark = true;
                    continue;
                }
                return Err(anyhow!(
                    "图生视频接口返回 {}:{}",
                    status.as_u16(),
                    truncate(&raw, 300)
                ));
            }
            let task: VideoTask = serde_json::from_str(&raw)
                .with_context(|| format!("图生视频响应里没有任务号:{}", truncate(&raw, 200)))?;
            if task.id.trim().is_empty() {
                return Err(anyhow!("图生视频接口没给任务号:{}", truncate(&raw, 200)));
            }
            // 尺寸写进日志:出片尺寸和这里报的不是一回事时(服务商可能自己改),这一行是唯一线索
            tracing::info!(
                "图生视频任务已提交:{}(要 {size} · {VIDEO_QUALITY} · {})",
                task.id,
                if watermark { "带水印" } else { "无水印" }
            );
            return Ok(task.id);
        }
    }

    /// 问一次任务跑到哪了(见 `Videos` 的说明:判成功看 body,不看 HTTP 码)。
    pub fn poll(&self, id: &str) -> Result<VideoProgress> {
        let mut response = self
            .agent
            .get(&format!("{}/async-result/{}", self.root(), id))
            .header("Authorization", &format!("Bearer {}", self.cfg.api_key))
            .call()
            .map_err(|err| anyhow!("查询图生视频结果失败:{err}"))?;
        let status = response.status();
        let raw = response.body_mut().read_to_string().context("读查询响应失败")?;
        let task: VideoTask = match serde_json::from_str(&raw) {
            Ok(task) => task,
            Err(err) => {
                // 解析不出任务形状时,HTTP 码才有意义(它多半是个真正的错误响应)
                return Err(anyhow!(
                    "查询图生视频结果失败(HTTP {}):{}",
                    status.as_u16(),
                    truncate(&raw, 300)
                ))
                .with_context(|| format!("{err}"));
            }
        };
        match task.task_status.as_str() {
            "SUCCESS" => {
                let video_url = task
                    .video_result
                    .first()
                    .map(|entry| entry.url.clone())
                    .unwrap_or_default();
                if video_url.trim().is_empty() {
                    return Err(anyhow!("服务商说成功了,但没给视频地址"));
                }
                Ok(VideoProgress::Done {
                    video_url,
                    cover_url: task
                        .video_result
                        .first()
                        .map(|entry| entry.cover_image_url.clone())
                        .unwrap_or_default(),
                })
            }
            "FAIL" => {
                // **全文**打出来。服务商在 FAIL 的时候一个字都不解释(只有个空 message),
                // 而 200 字的截断只够发一句给用户看的提示、不够定位问题 —— 2026-10-16 22:19
                // 那次失败被截在 `{"choices":[{"finish_reason":"stop",...` 就没了,查了三轮
                // 都没看见全文。这一行是"免费的复盘入口":任务号在手,`--video-fetch` 能反复
                // 问同一个任务,所以现场必须留在日志里。
                tracing::warn!("图生视频 FAIL(任务 {id})完整正文:{}", raw.trim());
                Ok(VideoProgress::Failed(truncate(&raw, 200)))
            }
            // PROCESSING 是实测见到的;别的状态一律当"还在跑"—— 宁可多等一轮,
            // 也不要把一个未知状态当成失败把任务扔掉(它可能已经把额度花了)。
            other => {
                tracing::debug!("图生视频还在跑:{other}");
                Ok(VideoProgress::Running)
            }
        }
    }

    /// 整条路:提交 → 轮询到出片 → **当场下载**。
    pub fn animate(
        &self,
        image_url: &str,
        prompt: &str,
        screen: (u32, u32),
    ) -> Result<GeneratedVideo> {
        self.animate_with(image_url, prompt, false, screen)
    }

    /// 同上,但**先按首尾帧提交**(首尾都给这张图)。
    ///
    /// **不要用它做壁纸。** 2026-10-17 的多轮对照出片证明"首尾同图"会把运动锁死:
    /// 同一张图、提示词都要求实体运动时,首尾帧 0.06,单图 quality 2.92 / speed 8.79
    /// (光流均值)。壁纸现在走 `animate`(单图首帧),接缝由播放器的 0.8 秒淡回第一帧兜住。
    ///
    /// 保留它是因为接口能力本身还在、诊断时可能要复现旧行为;不是给产品路径用的。
    pub fn animate_looped(
        &self,
        image_url: &str,
        prompt: &str,
        screen: (u32, u32),
    ) -> Result<GeneratedVideo> {
        self.animate_with(image_url, prompt, true, screen)
    }

    fn animate_with(
        &self,
        image_url: &str,
        prompt: &str,
        looped: bool,
        screen: (u32, u32),
    ) -> Result<GeneratedVideo> {
        let id = if looped {
            match self.submit_looped(image_url, prompt, screen) {
                Ok(id) => {
                    // 这一行是**故意**留的:首尾帧那条路会静默降级成单图重提,而"库里这段片子
                    // 到底是不是按闭环提交的"事后只能靠日志分辨(降级会打那记 warn)。
                    tracing::info!("首尾帧被受理(首尾都给同一张图 → 画面绕回起点)");
                    id
                }
                Err(err) => {
                    tracing::warn!("首尾帧这条路没被受理({err:#}),退回单图重提一次");
                    self.submit(image_url, prompt, screen)?
                }
            }
        } else {
            self.submit(image_url, prompt, screen)?
        };
        self.wait_and_download(&id)
    }

    /// 等一个**已经提交过**的任务出片,然后当场下载。
    ///
    /// 单独拎出来是因为它有两个入口:提交之后接着等(`animate_with`),
    /// 以及照着任务号把**超时丢掉的那一条**捡回来(`fetch`)。
    fn wait_and_download(&self, id: &str) -> Result<GeneratedVideo> {
        let started = std::time::Instant::now();
        loop {
            match self.poll(id)? {
                VideoProgress::Running => {
                    if started.elapsed() > VIDEO_WAIT_TIMEOUT {
                        return Err(anyhow!(
                            "等了 {:.0} 分钟还没出片(任务 {id} 可能还在排队)。\
                             它还在服务商那边跑、钱也已经花了,别重提 —— \
                             过一会儿用 `--video-fetch {id} <库 id>` 把它取回来",
                            VIDEO_WAIT_TIMEOUT.as_secs_f32() / 60.0
                        ));
                    }
                    std::thread::sleep(VIDEO_POLL_INTERVAL);
                }
                VideoProgress::Done { video_url, .. } => {
                    let bytes = self.download(&video_url)?;
                    tracing::info!(
                        "图生视频完成:{} KB · 耗时 {:.1}s",
                        bytes.len() / 1024,
                        started.elapsed().as_secs_f32()
                    );
                    return Ok(GeneratedVideo {
                        url: video_url,
                        bytes,
                    });
                }
                VideoProgress::Failed(reason) => {
                    // 把任务号写进报错:实测(2026-10-16/17)`task_status: FAIL` 的正文里
                    // **一个字的解释都没有**(只有一个空 message),想知道为什么挂、或者去要回
                    // 那 1 元,手里只有这个号。原样贴的 `reason` 留在后面当"现场"。
                    //
                    // "等了多久"必须一起写:2026-10-17 那几次是**熬了 10 分钟**才 FAIL,而成功
                    // 那次只用了 10.4 秒。等很久才失败基本就是服务商那边排队/过载(同一时段拿
                    // 另一个视频模型探,提交直接被 429 挡回来:code 1305「访问量过大」),
                    // 不是请求体写错了 —— 报错里得让用户能分辨这一点。
                    let waited = started.elapsed().as_secs();
                    return Err(anyhow!(
                        "服务商说这条视频没生成出来(任务 {id},等了 {waited} 秒,他们没给原因):{reason}\
                         —— 拖这么久才 FAIL 多半是他们那边排队/过载。过一会儿用 \
                         `--video-fetch {id} <库 id>` 免费取回,**别重提**(重提要再花一次钱)"
                    ));
                }
            }
        }
    }

    /// 按**任务号**取回一条已经提交过的视频:不提交、不再花钱。
    ///
    /// 为什么需要它:被超时抛弃的那条任务,服务端**还在跑、而且已经计过费** ——
    /// 重提一次就是又花 1 元买同一件事。任务号在日志和报错里都留着,照它捡回来就行。
    pub fn fetch(&self, id: &str) -> Result<GeneratedVideo> {
        tracing::info!("按任务号取回:{id}(不提新任务、不再计费)");
        self.wait_and_download(id)
    }

    /// 把视频抓成字节。走同一个 agent(超时和 TLS 配置都现成)。
    fn download(&self, url: &str) -> Result<Vec<u8>> {
        let mut response = self
            .agent
            .get(url)
            .call()
            .map_err(|err| anyhow!("下载视频失败:{err}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow!("下载视频返回 {}:{}", status.as_u16(), truncate(url, 120)));
        }
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_DOWNLOAD_BYTES)
            .read_to_vec()
            .context("读视频字节失败")?;
        if bytes.is_empty() {
            return Err(anyhow!("下载到的视频是空的:{}", truncate(url, 120)));
        }
        Ok(bytes)
    }
}

/// 把一张图的字节包成 `data:` URL(base64 内联)。
///
/// 为什么非要内联,而不是把服务商返回的那个图片地址丢给它:
/// - 生图返回的地址**只有 7 天**,视频那条更短(**24 小时**)。而库里的图是要长期留着的 ——
///   存地址等于存一个"过几天就打不开"的入口,于是"把上周生成的那张做成动态壁纸"就永远做不到。
/// - 内联之后**库里任意一张图**都能做视频,和它是哪天生成的、链接过期没有,完全无关。
///
/// 实测:2 MB 的请求体(1920×1088 的 PNG)照收,所以原图直接发,不缩也不重编码 ——
/// 重压一次只会让画面更糊,而这是**图生视频的条件图**,糊了整段视频都跟着糊。
pub fn image_data_url(bytes: &[u8]) -> String {
    // 只认这两种:服务商给的图不是 PNG 就是 JPEG(和 `photos.rs` 认扩展名同一个口径)
    let mime = if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else {
        "image/png"
    };
    format!("data:{mime};base64,{}", base64(bytes))
}

/// 标准 base64(带 `=` 补齐)。
///
/// 自己写而不是引一个依赖:算法就是下面这二十行,而这里已经用测试把它钉住了
/// (`base64_编码和已知向量一致`)。为了一处调用往依赖树里加一个 crate,不划算。
fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let first = chunk[0] as u32;
        let second = chunk.get(1).copied().unwrap_or(0) as u32;
        let third = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (first << 16) | (second << 8) | third;
        out.push(TABLE[(triple >> 18) as usize & 0x3F] as char);
        out.push(TABLE[(triple >> 12) as usize & 0x3F] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(triple >> 6) as usize & 0x3F] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[triple as usize & 0x3F] as char
        } else {
            '='
        });
    }
    out
}

/// 这个尺寸能不能原样发给服务商(三条规则见 `SIZE_MIN` 那段)。
///
/// 单独抽出来是给 `fit_size` 当**收尾那道闸**:尺寸是算出来的,而算错一次不是降级,
/// 是整轮白等一分钟(用户等完什么也没拿到),所以宁可让自检构建当场 panic。
/// `debug_assert` 只活在 debug 里,同一条规则还有一整套测试盯着(见 `屏幕尺寸总能折成合法请求尺寸`)。
fn size_is_legal(width: u32, height: u32) -> bool {
    width % SIZE_STEP == 0
        && height % SIZE_STEP == 0
        && (SIZE_MIN..=SIZE_MAX).contains(&width)
        && (SIZE_MIN..=SIZE_MAX).contains(&height)
        && width as u64 * height as u64 <= SIZE_PIXEL_MAX
}

/// 把"屏幕分辨率"折成一个**屏幕多大就要多大**的请求尺寸。
///
/// 为什么不留推荐档的余地:推荐档那股风是当天临时刮的(§11.26 记了起因 —— 怕自己算错被 400 拒),
/// 但它有一笔立刻能看见的代价:2560×1600 的屏离最近的一档(1568×1056)还差 **1.63 倍**,
/// 而接口本来就允许 2560×1600。用户一句「屏幕多大就要多大」把这条摆平了:图按屏的原生像素出、
/// 上屏 1:1,§11.24 末尾那 15% 的放大损失就不再是"必须付"的了。
///
/// 四条规则**按顺序**过(顺序本身是结论,不是随手排的):
/// 1. 就近对齐到 `SIZE_STEP`:1920×1080 → 1920×**1088**(和 16 倍数那版同一笔账,多补几行比裁掉好);
/// 2. 长边超 `SIZE_MAX` 就**等比**缩 —— 单独夹一个维度会把 16:9 压成 1.32
///    (3840×2176 夹成 2880×2176),比例毁了比小一圈更难看;
/// 3. 总像素超 2²² 同样等比缩;
/// 4. 短边小于 `SIZE_MIN` 等比放大(小屏 / 窄窗口才会走到,512 是接口自己的下限)。
/// 最后一次性落地到 32 的倍数(四舍五入),舍上去万一顶破总像素天花板就再等比退一次。
///
/// 三个能直接对照的数:`1920×1080 → 1920×1088`、`2560×1600 → 2560×1600`(**就是屏幕本身**)、
/// `3840×2160 → 2720×1536`(两条天花板都撞上,只能等比缩,比例仍在 16:9 附近)。
///
/// 返回值**保证合法**,收尾那记 `debug_assert` 盯着的就是这句话。
pub fn fit_size(screen_width: u32, screen_height: u32) -> (u32, u32) {
    let mut width = snap_alignment(screen_width) as f64;
    let mut height = snap_alignment(screen_height) as f64;

    // ② 长边天花板。等比缩,不单独夹一个维度。
    let longest = width.max(height);
    if longest > SIZE_MAX as f64 {
        let k = SIZE_MAX as f64 / longest;
        width *= k;
        height *= k;
    }

    // ③ 总像素天花板。
    if width * height > SIZE_PIXEL_MAX as f64 {
        let k = (SIZE_PIXEL_MAX as f64 / (width * height)).sqrt();
        width *= k;
        height *= k;
    }

    // ④ 下界:谁都不能小于 512。
    let shortest = width.min(height);
    if shortest < SIZE_MIN as f64 {
        let k = SIZE_MIN as f64 / shortest;
        width *= k;
        height *= k;
    }

    // ⑤ 落地:四舍五入到 32 的倍数。
    //
    // 这里**不能**用 `floor`(第一版就是这么写的,当场翻车):3440×1440 经第 ② 步正好落到
    // 2880×1200,而 1200/32 = 37.5 在浮点里是 37.500000000000004,`floor` 把它砍成 37 → 1184,
    // 白丢 16 像素、比例也跟着歪。而 1200 本来就不是 32 的倍数,取整方向只能靠"哪个比例更正"定,
    // 不靠某条舍入规则定 —— 所以这里就是普通的四舍五入,舍上去顶破天花板了再由下面那道等比退兜住。
    let mut width = (width / SIZE_STEP as f64).round() as u64 * SIZE_STEP as u64;
    let mut height = (height / SIZE_STEP as f64).round() as u64 * SIZE_STEP as u64;
    // 舍上去可能把总像素顶过天花板(两边各加不到 16 像素,4.19M 那边约 +3%),
    // 那就再等比退一次。这一处**可以**放心向下取整:宁小不大,退完必定在线内。
    if width * height > SIZE_PIXEL_MAX {
        let k = (SIZE_PIXEL_MAX as f64 / (width * height) as f64).sqrt();
        width = ((width as f64 * k) / SIZE_STEP as f64 + 1e-9).floor() as u64 * SIZE_STEP as u64;
        height = ((height as f64 * k) / SIZE_STEP as f64 + 1e-9).floor() as u64 * SIZE_STEP as u64;
    }
    // 最后那道闸:前面全是浮点算的,再离谱也不能让不合法的尺寸发出去(最多退一两次,不会转圈)。
    while width * height > SIZE_PIXEL_MAX {
        if width >= height {
            width -= SIZE_STEP as u64;
        } else {
            height -= SIZE_STEP as u64;
        }
    }

    let (width, height) = (width as u32, height as u32);
    debug_assert!(
        size_is_legal(width, height),
        "算出了不合法的尺寸 {}×{}:规则见 SIZE_MIN 那段",
        width,
        height
    );
    (width, height)
}

/// 就近(不是只往上、也不是只往下)对齐到 `SIZE_STEP`。
///
/// 这里**只对齐、不夹**上下界 —— 上下界那两刀必须留给 `fit_size` 做**等比**的:
/// `3840×2176` 夹长边成 `2880×2176` 会把 1.77 压成 1.32(`640×480` 抬短边成 `640×512`
/// 同理),画面比例当场就毁了。`fit_size` 第 ②④ 步是等比缩 / 等比放,比例能保住。
///
/// 顺带说清两次取整为什么不一样:这里(第 ① 步)是**四舍五入**,要的是"最接近屏";
/// `fit_size` 第 ⑤ 步是**向下取整**,要的是"别越线"。这不是漏改,是分工。
///
/// 那个 `1 << 16` 只是让"对齐 + 后面几刀"整段都待在 u32 里(屏幕不会真有这么大),
/// 免得有人拿 `u32::MAX` 调进来把后面乘出溢出。
fn snap_alignment(value: u32) -> u32 {
    let step = SIZE_STEP as u64;
    let value = (value.max(1) as u64).min(1 << 16);
    (((value + step / 2) / step) * step).max(1) as u32
}

/// 会话用的客户端。`clone` 很便宜(Agent 内部是共享的),发请求时丢给后台线程就行。
#[derive(Clone)]
pub struct Client {
    cfg: Config,
    agent: ureq::Agent,
}

impl Client {
    pub fn new(cfg: Config) -> Self {
        // `http_status_as_error(false)` 是**故意**的:4xx/5xx 也当普通响应收下来,
        // 这样才能把服务商返回的错误正文(比如"model not found""quota exceeded")原样带给用户,
        // 不然只剩一个干巴巴的 404。
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .http_status_as_error(false)
            // **必须显式挑 native-tls**:ureq 的默认 provider 写的是 rustls,我们没启 rustls 特性,
            // 光开 `native-tls` 特性它照样会在 https 上 panic("provider is Rustls but feature is
            // not enabled")。native-tls 在 Windows 上就是 schannel → 走系统证书库。
            .tls_config(
                TlsConfig::builder()
                    .provider(TlsProvider::NativeTls)
                    .build(),
            )
            .build()
            .into();
        Self { cfg, agent }
    }

    /// 给人看的服务商 / 模型标识,写进日志和状态行。
    pub fn describe(&self) -> String {
        let host = self
            .cfg
            .base_url
            .split("//")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .unwrap_or(&self.cfg.base_url);
        let thinking = match self.cfg.thinking {
            Some(false) => " · 不思考",
            _ => "",
        };
        format!("{host} · {}{thinking}", self.cfg.model)
    }

    /// 看图用哪个模型(自检打印、设置页回显都要它)。
    pub fn vision_model(&self) -> &str {
        &self.cfg.vision_model
    }

    /// `base_url` 后面补 `/chat/completions`。写成完整的 endpoint 也认(免得有人贴整条 URL 上来)。
    fn endpoint(&self) -> String {
        let base = self.cfg.base_url.trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            base.to_string()
        } else {
            format!("{base}/chat/completions")
        }
    }

    /// 生图这条路的第一跳:一句话 → 生图提示词 + 动效配方。**同样不会返回 Err** ——
    /// 模型不争气时,退化成"拿用户原话去画",这条路必须走得通,否则用户敲了字什么都没发生。
    pub fn plan_photo(&self, request: &str, current: Option<&Photo>) -> PhotoDraft {
        let mut messages = vec![
            Message::system(PHOTO_SYSTEM_PROMPT.to_string()),
            Message::user(photo_request(request, current)),
        ];
        let mut calls = 0;
        let mut last_error = String::new();

        for attempt in 0..2 {
            calls += 1;
            let text = match self.chat(&messages) {
                Ok(text) => text,
                Err(err) => {
                    // 请求本身就没成(key 错、网络不通、额度用完),重试只是白等 45 s
                    last_error = format!("{err:#}");
                    break;
                }
            };
            match parse_photo_reply(&text) {
                Ok(plan) => {
                    tracing::info!(
                        "生图计划:{}({})| 动效={} | 提示词={}",
                        plan.title,
                        if plan.reply.is_empty() { "无回复" } else { &plan.reply },
                        plan.motion.summary(),
                        truncate(&plan.image_prompt, 80)
                    );
                    return PhotoDraft {
                        reply: plan.reply,
                        title: plan.title,
                        image_prompt: plan.image_prompt,
                        motion: plan.motion,
                        degraded: false,
                        calls,
                    };
                }
                Err(err) => {
                    last_error = format!("{err:#}");
                    tracing::warn!(
                        "模型这次给的生图计划用不了(第 {} 次):{last_error}",
                        attempt + 1
                    );
                    messages.push(Message::assistant(text));
                    messages.push(Message::user(format!(
                        "上面那份 JSON 有问题:{last_error}\n\
                         请只重新输出一个合法的 JSON 对象:不要解释、不要 markdown 代码块、不要多余字段。"
                    )));
                }
            }
        }

        tracing::warn!("生图提示词这条路没走通({last_error}),直接用用户原话出图");
        PhotoDraft {
            reply: format!(
                "模型没接上({}),先用你的原话去画一张。",
                friendly_reason(&last_error)
            ),
            title: mcwp_spec::photo::title_from_prompt(request),
            image_prompt: fallback_image_prompt(request),
            motion: MotionRecipe::default(),
            degraded: true,
            calls,
        }
    }

    /// 图生视频这条路的第一跳:一张**已经生好的图** → **一句"它该怎么动"的提示词**。
    ///
    /// 为什么还要过一次文本模型:图生视频的提示词和生图的提示词**不是一回事**。
    /// 生图那句说的是"画面里有什么"(`Photo::prompt` 里已经有了),而这条要说的是
    /// "它该怎么动" —— 镜头推拉、云在飘、水面起波纹、光影在变。这中间的翻译还是只能由
    /// 懂语言的那一方做:让模型看一眼那张图的描述再写一句,比在代码里硬套
    /// "slow camera push in" 贴题得多,而这一次调用很便宜(一句几十字)。
    ///
    /// 和 `plan_photo` 一样**绝不返回 Err**:用户点的是「做成动态壁纸」,
    /// 不能因为一句话没写出来就什么都不做 —— 写不出来就用 `fallback_motion_prompt`。
    pub fn plan_motion(&self, photo: &Photo) -> String {
        let messages = vec![
            Message::system(MOTION_SYSTEM_PROMPT.to_string()),
            Message::user(motion_request(photo)),
        ];
        match self.chat(&messages) {
            Ok(text) => {
                let prompt = clean_motion_reply(&text);
                if prompt.is_empty() {
                    tracing::warn!("模型回的动效提示词是空的,用通用运镜");
                    return fallback_motion_prompt().to_string();
                }
                tracing::info!("动效提示词:{prompt}");
                prompt
            }
            Err(err) => {
                tracing::warn!("动效提示词没写出来({err:#}),用通用运镜");
                fallback_motion_prompt().to_string()
            }
        }
    }

    /// **看图**写动效方案(元素级)。这条路才是"画面真的活起来"的那一步。
    ///
    /// 和 `plan_motion` 的分工:那个是"只有标题和提示词"时的**兜底**(看不到画面的模型
    /// 只能写"缓慢推近"),这个是主路 —— 图就在手边(`entry.image` 的字节),没理由不给它看。
    ///
    /// 也**绝不返回 Err**:分析不出来就回空方案,调用方会退回 `fallback_motion_prompt`,
    /// 用户点的那颗按钮总得有点动静。
    pub fn plan_motion_vision(&self, photo: &Photo, image_bytes: &[u8]) -> MotionPlan {
        let messages = vec![
            Message::system(VISION_SYSTEM_PROMPT.to_string()),
            Message::user_with_image(
                motion_vision_request(photo),
                image_data_url(image_bytes),
            ),
        ];
        let raw = match self.chat_as(&self.cfg.vision_model, &messages) {
            Ok(text) => text,
            Err(err) => {
                tracing::warn!("动效分析(看图)没跑成({err:#}),退回通用运镜");
                return MotionPlan::default();
            }
        };
        match serde_json::from_str::<MotionPlan>(extract_json(&raw).unwrap_or("{}")) {
            Ok(mut plan) => {
                plan.normalize();
                if plan.is_empty() {
                    tracing::warn!("动效分析没认出任何元素,退回通用描述:{}", truncate(&raw, 200));
                } else {
                    tracing::info!(
                        "动效方案:镜头={} · {}个元素 · scene {} 字 / caption {} 字 / 最终提示词 {} 字",
                        plan.camera,
                        plan.elements.len(),
                        plan.scene.chars().count(),
                        plan.caption.chars().count(),
                        video_prompt(&plan).chars().count()
                    );
                    if !plan.has_caption() {
                        tracing::warn!("视觉模型没给 caption,只能退回元素拼接:{}", truncate(&raw, 200));
                    }
                }
                plan
            }
            Err(err) => {
                // 这一步的失败几乎全是形状问题(数组写成字符串、字段名写错),原话留在日志里
                tracing::warn!("动效分析的 JSON 读不懂({err}),退回通用运镜:{}", truncate(&raw, 200));
                MotionPlan::default()
            }
        }
    }

    /// 发一次 chat/completions,取回模型那段文本。
    fn chat(&self, messages: &[Message]) -> Result<String> {
        self.chat_as(&self.cfg.model, messages)
    }

    /// 同上,但**换一个模型名**。看图那一步就是靠它:地址和 key 还是对话那套,模型换成视觉档。
    ///
    /// 模型名单独当参数,而不是临时改 `self.cfg.model`:那个字段是"这次要跟谁说话"的真相,
    /// 改它会让并发的另一条路(比如同一轮里的生图降级)跟着串味。
    fn chat_as(&self, model: &str, messages: &[Message]) -> Result<String> {
        let body = serde_json::to_string(&ChatRequest {
            model,
            messages,
            temperature: 0.7,
            stream: false,
            thinking: self.cfg.thinking.map(Thinking::new),
        })?;
        let mut response = self
            .agent
            .post(&self.endpoint())
            .header("Content-Type", "application/json")
            .header("Authorization", &format!("Bearer {}", self.cfg.api_key))
            .send(body.as_str())
            .map_err(|err| anyhow!("请求失败:{err}"))?;
        let status = response.status();
        let raw = response.body_mut().read_to_string().context("读响应失败")?;
        if !status.is_success() {
            return Err(anyhow!(
                "服务商返回 {}:{}",
                status.as_u16(),
                truncate(&raw, 300)
            ));
        }
        let parsed: ChatResponse = serde_json::from_str(&raw)
            .with_context(|| format!("响应不是 OpenAI 格式:{}", truncate(&raw, 200)))?;
        let content = parsed
            .choices
            .into_iter()
            .next()
            .map(|choice| choice.message.content)
            .filter(|content| !content.trim().is_empty())
            .ok_or_else(|| anyhow!("模型返回了空内容"))?;
        // 模型原话留一份到 debug 日志。解析失败时这是**唯一**能看清"它到底写了什么"的地方,
        // 而这一步的失败几乎全是形状问题(数组写成了字符串、数字写成了字符串)。平时不吵:
        // 这条在 debug 级,`RUST_LOG=info,mcwp_ai=debug` 才出来。
        tracing::debug!("模型原文:{content}");
        Ok(content)
    }
}

/// 从模型那段文本里抠出 JSON 对象。
///
/// 提示词里写了"不要代码块",但免费模型经常还是裹一层 ```json —— 所以先拆围栏,再按
/// **第一个 `{` 到最后一个 `}`** 截:前后那句"好的,这是你要的规格:"也就一起被剥掉了。
///
/// 这条路对**提示词那一步**是必须的:模型的 `motion` 字段偶尔会写成裸字符串,
/// 整份 JSON 仍然是好的,所以"能抠出一个对象"就已经成功了一半。
fn extract_json(text: &str) -> Result<&str> {
    let start = text.find('{').ok_or_else(|| anyhow!("回复里没有 JSON"))?;
    let end = text.rfind('}').ok_or_else(|| anyhow!("JSON 没有闭合"))?;
    if end <= start {
        return Err(anyhow!("JSON 结构不完整"));
    }
    Ok(&text[start..=end])
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// 环境变量里的布尔:`1` / `true` / `on` / `yes` 和 `0` / `false` / `off` / `no` 都认。
///
/// **认不出来就返回 `None`**(于是继续往下走 `ai.json`、再走内置默认),而不是替用户挑一个。
/// 写成 `unwrap_or(false)` 是那种最烦人的错法:`MCWP_IMAGE_WATERMARK=0off` 这类手滑会
/// 静默变成"关掉水印",而用户以为自己已经关上了。
fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}

/// 截断到 `max` 个字符(按字符数,不按字节 —— 中文按字节截会切出半个字)。
fn truncate(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        format!("{}…", flat.chars().take(max).collect::<String>())
    } else {
        flat
    }
}

/// 把内部错误翻成一句人话,写进降级回复里。
///
/// 宁可粗一点,也别把服务商那坨 JSON 糊到面板上 —— 用户要的是"我该去改哪儿",细节在日志里
/// (`RUST_LOG=info` 就能看到完整那条)。这里按关键字认,认不出就说"原因见日志",不猜。
fn friendly_reason(error: &str) -> String {
    if error.contains("401") || error.contains("403") {
        "key 不对或没权限".to_string()
    } else if error.contains("404") {
        "模型名不对".to_string()
    } else if error.contains("429") {
        "额度或频率用完了".to_string()
    } else if error.starts_with("服务商返回") {
        "服务商那边出错了".to_string()
    } else if error.contains("超时") || error.contains("timed out") || error.contains("timeout") {
        "等太久了".to_string()
    } else if error.contains("请求失败") {
        "网络没通".to_string()
    } else if error.contains("JSON") || error.contains("OpenAI 格式") {
        "它回的格式读不懂".to_string()
    } else {
        "原因见日志".to_string()
    }
}

/// 模型配置文件的落点,和壁纸库、会话同一个目录。
fn config_file() -> Result<PathBuf> {
    Ok(std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("读不到 APPDATA,无法定位模型配置"))?
        .join("MC壁纸")
        .join("ai.json"))
}

/// 读盘上那份 `ai.json`。**读不懂就当没配**(返回 `None`),而不是把错误抛给调用方 ——
/// 用户能改的地方,坏文件不该让程序打不开,和会话 / 壁纸库那边一个态度。
fn read_stored() -> Option<Stored> {
    let path = config_file().ok()?;
    let text = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str::<Stored>(&text) {
        Ok(file) => Some(file),
        Err(err) => {
            tracing::warn!("ai.json 解析失败,这次当没配:{err}");
            None
        }
    }
}

/// 生图那条路的系统提示词。
///
/// **故意不用 `format!` 插值**:里面全是 JSON 示例的大括号,插值要写一屏的 `{{`/`}}`,
/// 改一次错一次。动效清单写成字面量,由测试 `提示词里的动效清单和枚举一致` 盯着它别和枚举漂开。
const PHOTO_SYSTEM_PROMPT: &str = "\
你是「MC 壁纸」的画面导演。用户会用一句话说他想要一张什么样的桌面壁纸。\
你要把这句需求**补全成一张具体的画面**,写成一句**给文生图模型的提示词**,再给这张图配一套动效。\n\
\n\
【题材只认「用户说」这一句】\n\
- 用户那句话就是要画的东西,**以它为准**;\n\
- 用户常常说得短、说得笼统(“生成一张 windows 系统的壁纸”“来张深色的”“安静一点的”),\
你要**替他补全** —— 补的是画面细节:主体长什么样、环境、光线、色调、构图、画风。\
**补全是把话说完,不是换题材**;\n\
- 例:用户说“生成一张 windows 系统的壁纸”,意思是要一张**用在 Windows 上**的舒心壁纸,\
照这个理解去补全画面;不是去画 Windows 的窗口和徽标(徽标、字样本来就在下面的禁用清单里);\n\
- 【当前壁纸】只是“他现在屏幕上正放着什么”,**单纯是参考**:只有用户说“换成 / 改成 / 再 / 更…”\
这类**相对说法**时,才拿它当参照;\n\
- 用户**没提到**的题材(雪山、湖泊、雨夜、城市……)—— 特别是当前壁纸里那些 —— \
**一律不许出现**在提示词里。把上一张画的内容抄进这一张,是最煞风景的错误。\n\
\n\
只输出一个 JSON 对象,不要解释、不要 markdown 代码块、不要多余字段,结构:\n\
{\n\
\x20 \"reply\": \"对用户说的一句话(30 字以内,口吻像「好,给你画一张雪山湖的清晨」;别复述提示词)\",\n\
\x20 \"title\": \"这张壁纸的短标题(6~12 字,别用书名号)\",\n\
\x20 \"image_prompt\": \"给文生图模型的提示词,写法见下\",\n\
\x20 \"motion\": { \"kind\": \"动效名\", \"intensity\": 0~1, \"zoom\": 0~1, \"pan\": 0~1, \"speed\": 0~1 }\n\
}\n\
\n\
image_prompt 的写法(这一句直接决定出图质量,请认真写):\n\
- 中文,40~90 字,**一个连贯的长句**,不要分点、不要换行、不要引号\n\
- 必须交代清楚:主体 + 环境 + 光线 + 色调 + 构图 + 画风\n\
- 画风往「高清数字插画 / 电影感摄影」这类稳的方向写,别写抽象概念和情绪词\n\
- **绝对不要出现**:文字、字母、数字、水印、logo、人脸特写、人物主体 —— 壁纸会被图标压住,\n\
\x20 出现文字和人脸只会让人觉得脏\n\
- 构图往「开阔、地平线偏低、中间偏空」写,给桌面图标留地方\n\
\n\
motion.kind 只能取下面之一(按题材挑,拿不准就用 still):\n\
- still  静帧呼吸:只是很慢地推近。**任何题材都安全,拿不准就选它**\n\
- snow   落雪:雪景、冬天、寒冷、严寒\n\
- rain   雨丝:下雨、城市夜景、潮湿、阴天\n\
- fog    雾气:山、森林、清晨、朦胧、远处\n\
- cloud  云影:天空、草原、旷野、开阔的户外\n\
- glow   光晕:灯光、日落、暖色、室内\n\
- ripple 水波:水面、湖泊、海底、倒影、雨后的水洼\n\
\n\
motion 的四个数都是 0~1:intensity 是效果强弱,zoom 是画面推近幅度,pan 是平移幅度,speed 是速度。\n\
**宁可小不要大** —— 壁纸是背景,动得太明显会让人分心,而且推得越快越容易让人觉得晃。\n\
推荐区间:intensity 0.4~0.6 / zoom 0.1~0.25 / pan 0.05~0.15 / speed 0.2~0.4。";

/// 生图那条路的用户消息。
///
/// **两块的地位不一样**:【当前壁纸】是"他现在屏幕上是什么",【用户说】才是这次的题材。
///
/// 这里的措辞是踩出来的:原来两块都只是平铺(当前壁纸在前、光秃秃一个"用户说"在后),
/// 小模型会把这种写法读成"要改的东西 + 一句要求",于是照着当前壁纸抄 —— 用户报过
/// "怎么生成的都是雪山湖面",连"生成一张 windows 系统的壁纸"都画成了雪山湖。现在把
/// "仅供参考 / 以这句为准"直接写进标题,比在代码里猜用户是不是"想改一改"可靠得多。
/// 写「动效提示词」用的系统提示词。
///
/// 和生图那份一样,**把底线写在提示词里而不是写在代码里**:图生视频的模型不认识我们的数据结构,
/// 它只听得到这句话。所以这里要说清楚三件事:写的是"怎么动"不是"画了什么"、只写一句、
/// 以及一串"绝对不要出现"的东西(文字 / 水印 / 分镜切换,都是这一段喂出来的视频常见的翻车点)。
const MOTION_SYSTEM_PROMPT: &str = "\
你在为「动态壁纸」写图生视频提示词。用户会给你一张**已经画好的图**的文字描述;\
你看不到像素,所以只能写描述里确实提到过的东西,不要编。\n\
只输出**一段英文**,55~85 个词,不要解释、不要加引号、不要 JSON、不要 markdown。\n\
必须满足:\n\
1. 第一句先点一下场景,紧接着写 \"The camera remains locked off.\";\n\
2. 找出描述里**能物理移动的 3~5 样东西**(云、雾、水面、树叶、草、花、车流、人群、\
   飞鸟、衣摆、头发、窗帘、烟……),每样都写清楚位置和一个现在时动词;\n\
3. 动词要具体、看得出来:drifts slowly right / sways back and forth / flutters /\n\
   ripples outward / rustles / billows / streams past / glides;\n\
4. 不要只写光影变化 —— 至少有两样是实体在动;\n\
5. 不要镜头切换、不要缩放/平移/环绕、不要出现新物体、不要文字、不要画风词;\n\
6. 最后一句写 \"Each motion completes one full visible cycle and returns to its starting pose; no camera movement, zoom, pan or orbit, and exposure and color stay fixed.\"。";

/// 交给模型的"这张图是什么"。
fn motion_request(photo: &Photo) -> String {
    format!(
        "这张壁纸叫「{}」,画面描述是:{}\n\
         请写一段英文的图生视频提示词:让描述里能动的实体真的动起来,\
         不要只写光影变化。",
        photo.title, photo.prompt
    )
}

/// 看图那一步给模型的话。
///
/// 标题和提示词**照样带上**,但只是"陪你看图"的旁白 —— 真正要说的是那句"请看图",
/// 否则模型很可能顺着提示词复述一遍画面,而不是去看像素(它连画面里有没有车都不知道)。
fn motion_vision_request(photo: &Photo) -> String {
    format!(
        "Image title (Chinese): 「{}」\n\
         Original generation prompt (Chinese): {}\n\
         Look at the image itself, not the text. The Chinese text is only context;\n\
         translate everything you see into English.\n\
         List 4-5 movers across sky, midground and foreground, with at least 3 physical objects.\n\
         Do not describe only changing light: leaves, grass, clouds, water, traffic, fabric\n\
         and hair must actually move. Output English only (except area).",
        photo.title, photo.prompt
    )
}

/// 看图写方案的题目。2026-10-17 重写:模型只输出**结构化的英文场景 + 英文实体动作**,
/// 最终那段英文长描述由本地按固定结构拼(见 `compose_scene_caption`)。
///
/// 为什么不再让模型直接写整段:
/// - CogVideoX 官方 i2v 要的是"从首帧开始的一段完整画面描述"(见 `inference/convert_demo.py`);
/// - 但免费视觉模型两次会漏掉不同的元素,还把 "camera remains locked off" 放到最后;
/// - 旧版中文短清单 + "所有动作必须来回" 又把动作压成"阳光一明一暗 / 树影摇摆"。
///
/// 现在模型只负责"看见什么、它怎么动"这两件它擅长的事,句子结构和顺序由本地保证。
const VISION_SYSTEM_PROMPT: &str = "\
你在为「动态壁纸」写**图生视频**提示词。用户会给你一张图,这张图就是视频的**第一帧**。\n\
先扫四个层次:天空 → 远景 → 中景 → 前景。找**能物理移动的东西**:\n\
云、雾、烟、雨雪、水面/波纹、树叶、树枝、草、蕨类、花、旗子、窗帘、衣摆、头发、\n\
人群、车流、飞鸟、昆虫、火焰、浮尘。\n\
然后**只输出一份 JSON**,不要解释、不要 markdown 代码块、不要多余的字:\n\
{\"camera\":\"固定机位\",\"scene\":\"English scene anchor, 12-24 words\",\"elements\":[{\"kind\":\"English noun phrase\",\"area\":\"左下\",\"action\":\"short English verb phrase\",\"sentence\":\"One complete present-tense English sentence, 8-16 words\"}]}\n\
规则:\n\
1. **只写图里看得见的东西**。图里没有车就别写车流;编出来的动作会让视频里凭空出现东西。\n\
2. `area` 只能逐字使用这十个中文值之一:左上、上中、右上、左中、正中、右中、左下、下中、右下、全图。\n\
   不要写 sky / foreground / midground / center 这类英文,**中文值写错这条元素就无效**。\n\
3. **必须列 4~5 个元素**,其中**至少 3 个是实体**(云/水/植物/车/人/鸟/烟/衣摆……)。\n\
   太阳、夕阳、光线、光晕、阴影、曝光、亮度、色彩变化都**不算实体**,不要把它们写成元素。\n\
   画面里真的只有两三样会动,就写两三样,但**至少 2 个实体**,不能只写光影。\n\
4. **元素顺序前景优先**:先写草、蕨类、花、衣摆等前景,再写树/车/人群等中景,\n\
   最后写云/鸟等天空元素。前景有植物或地面,至少写一个前景元素。\n\
5. `scene` 写 **12~24 个英文词**:地点 + 地形 + 前景/中景/天空的主要实体,\n\
   例如 \"A dense forest with a winding dirt path, thick ferns and tall trees on both sides\";\n\
   不要写时间、日落、阳光、光晕、阴影、曝光、色调或 mood,也不要写 \"English\" 这个词。\n\
   `scene`、`kind`、`action` 必须是英文,中文标题和中文描述只是上下文,不要用中文回答。\n\
6. `sentence` 是**每个元素一句完整英文**,8~16 词,主语 + 现在时强动词 + 方向/幅度。\n\
   必须写出方向或幅度:back and forth / from left to right / outward / upward /\n\
   in large waves / deeply / strongly / continuously / across the frame。\n\
   不要只写\"sway gently\",要写\"sway strongly back and forth\";\n\
   草和蕨类写 bends deeply / ripples in visible waves,树叶写 shakes and rustles continuously,\n\
   云写 streams steadily across the sky。\n\
   例:Thin clouds stream steadily to the left across the orange sky.\n\
   语法必须正确(grass 用 bends/ripples,不用 bend/ripple)。\n\
   **不要用 subtle / subtly / tiny / faint / slight / gentle / gently / softly /\n\
   gracefully** 形容主要动作;也不要写\"保持不变\"\"静止\"来凑数。\n\
   `action` 只是给诊断看的短动词短语,caption 真正用的是 `sentence`。\n\
7. `camera` 一律写\"固定机位\":壁纸不需要运镜,画面里的东西自己动才是重点。\n\
8. 不要镜头切换、不要缩放/平移/环绕、不要出现新物体、不要文字、不要画风词。";

/// 把元素级方案拼成**图生视频那句提示词**。
///
/// 削掉动作开头和元素名重合的那一段。
///
/// 模型经常把元素名也写进 `action`(提示词里交代过"别重复",它不一定听):
/// `kind="灯光"` + `action="灯光逐个亮灭闪烁"`,还有更隐蔽的
/// `kind="云朵"` + `action="云向右缓慢飘移"` —— 重合的只有"云"一个字。
/// 拼成"左上云朵云向右缓慢飘移"读着还行,但视频模型看到两个"云"容易当成两处云。
///
/// 所以按**尽可能长的重合前缀**削掉,而且削完不能是空的:宁可留着重复,也不能把动作削没。
fn trim_repeated_prefix<'a>(kind: &str, action: &'a str) -> &'a str {
    let mut overlap = 0;
    for (index, ch) in kind.char_indices() {
        // 切点必须落在**字符边界**上:`char_indices` 给的是每个字符的起始字节,
        // 结束位置得再加一个字长,不然会切进汉字中间(直接 panic)
        let end = index + ch.len_utf8();
        if action.starts_with(&kind[..end]) {
            overlap = end;
        }
    }
    if overlap > 0 && action.len() > overlap {
        &action[overlap..]
    } else {
        action
    }
}
///
/// 把动效方案变成**发给视频模型的那一句**。
///
/// 主路是 `plan.caption`:视觉模型写好的英文长描述。为什么不再本地拼 ——
/// 官方 CogVideoX 的 i2v 提示词是"从首帧开始的一段完整画面描述",元素清单拼出来的
/// 短句没有足够动作语义,实测只会让画面做光影呼吸。
///
/// `caption` 为空时才退回元素拼接(老资产 / 模型漏字段),再空就退回通用描述。
pub fn video_prompt(plan: &MotionPlan) -> String {
    if plan.is_empty() {
        return fallback_motion_prompt().to_string();
    }
    let caption = plan.caption.trim();
    if !caption.is_empty() {
        return fit_video_caption(caption);
    }
    // 新方案:模型给 scene + 英文 elements,本地拼成官方要的英文长描述。
    if !plan.scene.trim().is_empty() && !plan.elements.is_empty() {
        return compose_scene_caption(plan);
    }
    let camera = if plan.camera.trim().is_empty() {
        "固定机位"
    } else {
        plan.camera.trim()
    };
    let elements: Vec<String> = plan
        .elements
        .iter()
        .map(
            |MotionElement {
                 kind, area, action, ..
             }| {
                // 动作里那个重复的元素名要先削掉,不然会拼出"正中灯光灯光逐个亮灭闪烁"
                let action = trim_repeated_prefix(kind, action);
                format!("{area}{kind}{action}")
            },
        )
        .collect();
    let head = format!("镜头{camera}。画面里:");
    let tail = "。动作连续、轻缓,构图、色彩和光线保持稳定,结尾回到起始画面。";
    let budget =
        MAX_VIDEO_PROMPT_CHARS.saturating_sub(head.chars().count() + tail.chars().count() + 1);
    format!("{head}{}{tail}", truncate(&elements.join(";"), budget))
}

/// 把 `scene` + 英文 `elements` 拼成一段完整的英文图生视频描述。
///
/// 顺序是固定的:
/// 1. 场景锚点(8~15 词);
/// 2. `The camera remains locked off.` —— 壁纸不做运镜;
/// 3. 每个元素一句 `In the <位置>, <元素> <动作>.`;
/// 4. 闭环句。
///
/// **先给闭环句留位置,再塞元素**:宁可少写一个元素,也不能让 512 字把
/// `MOTION_LOOP_TAIL` 截掉 —— 那是循环接缝的一半(见 §11.22)。
fn compose_scene_caption(plan: &MotionPlan) -> String {
    const TAIL: &str = MOTION_LOOP_TAIL;
    let scene = clean_scene_anchor(&plan.scene);
    let mut text = if scene.is_empty() {
        String::new()
    } else {
        format!("{scene}.")
    };
    if !text.is_empty() {
        text.push(' ');
    }
    text.push_str("The camera remains locked off.");

    // **前景优先**。视觉模型即使被要求"先前景",仍会把 clouds 写在第一句;
    // 视频模型对前几句最敏感,天空排前面就会只做云/光的变化。这里按位置强制排序,
    // 同一层内保留模型给的顺序。
    let mut elements: Vec<&MotionElement> = plan.elements.iter().collect();
    elements.sort_by_key(|element| motion_layer_order(element));
    let mut kept = 0usize;
    for element in elements {
        // 旧方案里可能有 `kind: Sunset / sentence: The sun sets...`。
        // 它会把视频模型带回"太阳落山、整体明暗变化";这种伪运动直接丢掉。
        if is_light_only_mover(element) {
            continue;
        }
        let kind = element.kind.trim().trim_end_matches(['.', '。']);
        let action = trim_repeated_prefix(kind, element.action.trim())
            .trim_end_matches(['.', '。'])
            .trim();
        // 优先用模型写的完整句(主谓一致);没有才退回 "位置 + 元素 + 动作"。
        let sentence = element.sentence.trim().trim_end_matches(['.', '。']).trim();
        let sentence = if !sentence.is_empty() {
            format!("{sentence}.")
        } else if !kind.is_empty() && !action.is_empty() {
            let area = area_phrase(&element.area);
            format!("{area}, {kind} {action}.")
        } else {
            continue;
        };
        let needed = text.chars().count() + 1 + sentence.chars().count() + 1 + TAIL.chars().count();
        if needed > MAX_VIDEO_PROMPT_CHARS {
            break;
        }
        text.push(' ');
        text.push_str(&sentence);
        kept += 1;
    }
    if kept == 0 {
        return fallback_motion_prompt().to_string();
    }
    text.push(' ');
    text.push_str(TAIL);
    truncate_at_sentence(&strengthen_motion(&text), MAX_VIDEO_PROMPT_CHARS)
}

/// 清理视觉模型给的场景锚点。
///
/// 免费模型实际返过 `English forest path at dusk` —— 它会把提示词里的
/// "English scene anchor" 当成正文抄下来;而 `at dusk / at sunset / warm sunlight`
/// 这类词会把视频模型往"太阳落山、整体明暗变化"上带,正是这次要修的病。
/// 这里只做确定性的清洗:去 `English` 前缀、去时间/光线短语、压空格、首字母大写。
fn clean_scene_anchor(raw: &str) -> String {
    let mut text = raw.trim().trim_matches(['"', '\u{201c}', '\u{201d}']).trim().to_string();
    if text
        .get(..7)
        .is_some_and(|head| head.eq_ignore_ascii_case("english"))
    {
        let rest = &text[7..];
        if rest.is_empty() || rest.chars().next().is_some_and(|ch| ch.is_whitespace()) {
            text = rest.trim_start().to_string();
        }
    }

    for phrase in [
        " at dusk",
        " at sunset",
        " at sunrise",
        " during sunset",
        " during sunrise",
        " in the evening",
        " in the morning",
        " at golden hour",
        " in golden hour",
        " with warm sunlight",
        " with sunlight",
        " in warm light",
    ] {
        loop {
            let lower = text.to_ascii_lowercase();
            let Some(at) = lower.find(phrase) else { break };
            text.replace_range(at..at + phrase.len(), "");
        }
    }

    let text = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(['.', '。', ',', '，'])
        .to_string();
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

/// 把模型爱用的"弱动作"副词换成更明确的词。
///
/// 免费视觉模型即使被明确禁止,还是会写 "sway gently" / "bend gracefully"。
/// 这些词会让视频模型理解成"几乎不动",正是用户看到的"只有光影在变"。
/// 只换这一小撮高频词,不动句子结构:
/// gently → steadily, softly → visibly, gracefully → rhythmically,
/// lazily → slowly, slightly → clearly, subtly → clearly, faintly → clearly。
fn strengthen_motion(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphabetic() || ch == '\'' {
            word.push(ch);
        } else {
            out.push_str(&stronger_word(&word));
            word.clear();
            out.push(ch);
        }
    }
    out.push_str(&stronger_word(&word));

    // 只把副词换强还不够。`grass bends rhythmically`、`clouds drift slowly` 这种句子
    // 语法正确但幅度听起来很小,视频模型就真的只给一点点位移。这里把最常见的弱动作
    // 短语换成能看出幅度的写法(见 §11.31 的对照出片:quality 档已锁住镜头,但草叶
    // 的局部位移偏小,需要把动词本身写重)。
    out.replace("bends rhythmically", "bends deeply")
        .replace("bend rhythmically", "bend deeply")
        .replace("sway steadily", "sway strongly")
        .replace("sway slowly", "sway strongly back and forth")
        .replace("drift slowly", "stream steadily")
        .replace("drift steadily", "stream steadily")
        .replace("rustle steadily", "rustle continuously")
        .replace("flutter steadily", "flutter continuously")
}

fn stronger_word(word: &str) -> &str {
    if word.is_empty() {
        return word;
    }
    match word.to_ascii_lowercase().as_str() {
        "gently" => "steadily",
        "softly" => "visibly",
        "gracefully" => "rhythmically",
        "lazily" => "slowly",
        "slightly" => "clearly",
        "subtly" => "clearly",
        "faintly" => "clearly",
        _ => word,
    }
}

/// 这个元素是不是"光"而不是实体。
///
/// 只看 `kind` 的英文词元,避免把 `sunflowers`(向日葵)误判成 `sun`。
/// `light breeze` 这种形容词也不会被误伤,因为这里看的是元素名。
fn is_light_only_mover(element: &MotionElement) -> bool {
    let kind = element.kind.to_ascii_lowercase();
    kind.split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .any(|token| {
            matches!(
                token,
                "sun"
                    | "sunlight"
                    | "sunset"
                    | "sunrise"
                    | "glow"
                    | "light"
                    | "lighting"
                    | "shadow"
                    | "shadows"
                    | "exposure"
                    | "brightness"
                    | "color"
                    | "colour"
                    | "flare"
            )
        })
}

/// 运动元素所在的画面层次:0 前景 / 1 中景 / 2 天空。
///
/// 视觉模型有时给中文九宫格,有时直接给 `foreground / midground / sky`;两种都认。
/// 认不出来时看元素名(草、蕨、花 vs 云、鸟),再不行就放中景。
fn motion_layer_order(element: &MotionElement) -> u8 {
    let area = element.area.trim().to_ascii_lowercase();
    if matches!(area.as_str(), "左下" | "下中" | "右下")
        || area.contains("foreground")
        || area.contains("front")
        || area.contains("lower")
    {
        return 0;
    }
    if matches!(area.as_str(), "左上" | "上中" | "右上")
        || area.contains("sky")
        || area.contains("upper")
        || area.contains("background")
    {
        return 2;
    }
    if matches!(area.as_str(), "左中" | "正中" | "右中" | "全图")
        || area.contains("midground")
        || area.contains("middle")
        || area.contains("center")
    {
        return 1;
    }

    let kind = element.kind.to_ascii_lowercase();
    if ["grass", "fern", "flower", "bush", "fabric", "hair"]
        .iter()
        .any(|word| kind.contains(word))
    {
        return 0;
    }
    if ["cloud", "bird", "smoke", "sky"]
        .iter()
        .any(|word| kind.contains(word))
    {
        return 2;
    }
    1
}

/// 九宫格位置 → 英文句首。认不出来的(模型偶尔给英文)就原样用。
fn area_phrase(area: &str) -> String {
    match area.trim() {
        "左上" => "In the upper left".to_string(),
        "上中" => "Across the upper center".to_string(),
        "右上" => "In the upper right".to_string(),
        "左中" => "Along the left side".to_string(),
        "正中" => "In the center".to_string(),
        "右中" => "Along the right side".to_string(),
        "左下" => "In the lower left".to_string(),
        "下中" => "Across the lower center".to_string(),
        "右下" => "In the lower right".to_string(),
        "全图" => "Across the whole frame".to_string(),
        other if !other.is_empty() => other.to_string(),
        _ => "In the scene".to_string(),
    }
}

/// 视频接口对 `prompt` 的硬上限是 512 字符。Caption 超了不能直接拦腰砍:
/// 半句英文会让模型把后半截当成新指令。优先在最后一个句号处收口。
fn truncate_at_sentence(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut acc = String::new();
    let mut last_end_bytes = 0usize;
    let mut last_end_chars = 0usize;
    let mut count = 0usize;
    for ch in text.chars() {
        if count >= max {
            break;
        }
        acc.push(ch);
        count += 1;
        if matches!(ch, '.' | '!' | '?' | '。' | '！' | '？') {
            last_end_bytes = acc.len();
            last_end_chars = count;
        }
    }
    // 至少留一半内容才值得按句子收;否则硬截 + 省略号,免得只剩一个短句。
    if last_end_chars * 2 >= max {
        acc.truncate(last_end_bytes);
    } else {
        while acc.chars().count() + 1 > max {
            acc.pop();
        }
        acc.push('…');
    }
    acc
}

/// 把视觉模型给的 `caption` 收成一条能直接发的提示词。
///
/// 三件事必须兜住:
/// - 模型爱加 ``` 和引号,直接发过去会变成画面里的怪东西;
/// - 没有 "camera" 就补一句固定机位 —— 壁纸不需要运镜;
/// - 没有闭环句就补一句,接缝才不容易被看出来。
fn fit_video_caption(raw: &str) -> String {
    let mut body = raw.trim();
    if let Some(rest) = body.strip_prefix("```json") {
        body = rest;
    } else if let Some(rest) = body.strip_prefix("```") {
        body = rest;
    }
    if let Some(rest) = body.strip_suffix("```") {
        body = rest;
    }
    let mut text = body
        .trim()
        .trim_matches(['\"', '\u{201c}', '\u{201d}'])
        .trim()
        .to_string();
    let lower = text.to_ascii_lowercase();
    if !lower.contains("camera") && !text.contains("镜头") {
        text = format!("The camera remains locked off. {text}");
    }
    let lower = text.to_ascii_lowercase();
    if !lower.contains("full visible cycle") && !text.contains("无缝")
    {
        const TAIL: &str = MOTION_LOOP_TAIL;
        let budget = MAX_VIDEO_PROMPT_CHARS.saturating_sub(TAIL.chars().count() + 1);
        text = truncate_at_sentence(&text, budget);
        text.push(' ');
        text.push_str(TAIL);
    }
    truncate_at_sentence(&strengthen_motion(&text), MAX_VIDEO_PROMPT_CHARS)
}

/// 模型没答上来时用的通用描述。**必须是一段真能用的话**:这条路是兜底,
/// 用户点完按钮总得有东西动起来,而不是弹一句"模型没接上"。
///
/// 旧版是"镜头缓慢推近 + 云/水/光影"的中文短句;现在改成英文长描述,和主路同一个形状。
/// 用 "Where present" 限定,是怕图里没有水面/烟时模型硬加。
pub fn fallback_motion_prompt() -> &'static str {
    "The camera remains locked off on this scene. Where present, thin clouds drift across the sky, tree canopies and leaves sway and rustle, grass and ferns bend and spring back, water surfaces ripple outward, and loose fabric, hair or smoke moves with the air. Each motion completes one full visible cycle and returns to its starting pose; no camera movement, zoom, pan or orbit, and exposure and color stay fixed."
}

/// 把模型那句回复收干净:去围栏、去引号、把换行接成一段、限长。
///
/// 免费模型很爱加一层 ``` 或者"提示词:"这样的前缀;而现在这条路要的是**一整段英文**,
/// 只取第一行会把 60 词的描述砍成半句。
fn clean_motion_reply(text: &str) -> String {
    const PREFIXES: [&str; 6] = [
        "提示词:",
        "提示词：",
        "输出:",
        "输出：",
        "Prompt:",
        "prompt:",
    ];
    let mut picked = String::new();
    for raw in text.lines() {
        let mut line = raw.trim().trim_start_matches(['#', '*', '-']).trim();
        if line.starts_with("```") {
            continue;
        }
        for prefix in PREFIXES {
            if let Some(rest) = line.strip_prefix(prefix) {
                line = rest.trim();
                break;
            }
        }
        let line = line.trim_matches(['"', '\u{201c}', '\u{201d}']).trim();
        if line.is_empty() {
            continue;
        }
        if !picked.is_empty() {
            picked.push(' ');
        }
        picked.push_str(line);
    }
    truncate(&picked, MAX_MOTION_PROMPT_CHARS)
}

pub fn photo_request(request: &str, current: Option<&Photo>) -> String {
    match current {
        Some(photo) => format!(
            "【当前壁纸】(仅供参考,不是这次的题材)\n标题:{}\n提示词:{}\n动效:{}\n\n\
             【用户说】(以这句为准)\n{}",
            photo.title,
            photo.prompt,
            photo.motion.summary(),
            request
        ),
        None => format!("【用户说】(以这句为准)\n{request}"),
    }
}

/// 降级用的提示词:用户原话 + 一段固定的画风与底线要求。
///
/// 直接把用户原话丢给生图模型也能出图(`cogview` 吃中文),只是构图和画风听天由命。
/// 加上这一段,至少能把"不要文字、留白、画风"这三条底线兜住 —— 这三条恰恰是壁纸最容易翻车的地方。
fn fallback_image_prompt(request: &str) -> String {
    format!(
        "{},高清数字插画,电影感光线,柔和色调,构图开阔,中间留白,无文字无水印无人脸",
        request.trim()
    )
}

/// 模型该给出的"生图计划"。
#[derive(Debug, Default)]
struct PhotoPlan {
    reply: String,
    title: String,
    image_prompt: String,
    motion: MotionRecipe,
}

#[derive(Debug, Deserialize)]
struct PhotoReply {
    #[serde(default)]
    reply: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    image_prompt: String,
    /// 先当 `Value` 收着再单独解析:`motion` 是最容易写走样的一块
    /// (整块写成一句 `"snow"` 之类),那样不该让整次调用白费。
    #[serde(default)]
    motion: serde_json::Value,
}

/// 解析生图那段回复。和 `parse_reply` 一个态度:**形状走样就地修,不靠重试**。
fn parse_photo_reply(text: &str) -> Result<PhotoPlan> {
    let json = extract_json(text)?;
    let answer: PhotoReply = serde_json::from_str(json).context("不是约定的 JSON 结构")?;

    let image_prompt = truncate(&answer.image_prompt, MAX_IMAGE_PROMPT_CHARS);
    if image_prompt.is_empty() {
        // 这一条是**唯一**不能就地修的:没有提示词就没有图可画
        anyhow::bail!("image_prompt 是空的");
    }

    let mut motion = match answer.motion {
        // 整块写成一句字符串(比如 "snow"):认得出动效名就认,认不出就回默认
        serde_json::Value::String(kind) => MotionRecipe {
            kind: MotionKind::from_id(&kind).unwrap_or_default(),
            ..MotionRecipe::default()
        },
        serde_json::Value::Null => MotionRecipe::default(),
        // 字段写错类型 / 缺字段:回默认,而不是把这一轮废掉
        value => serde_json::from_value(value).unwrap_or_default(),
    };
    motion.normalize();

    let title = {
        let trimmed = answer.title.trim();
        if trimmed.is_empty() {
            mcwp_spec::photo::title_from_prompt(&image_prompt)
        } else {
            trimmed.chars().take(24).collect()
        }
    };
    let reply = {
        let trimmed = truncate(&answer.reply, MAX_REPLY_CHARS);
        if trimmed.is_empty() {
            format!("正在按「{title}」画一张。")
        } else {
            trimmed
        }
    };

    Ok(PhotoPlan {
        reply,
        title,
        image_prompt,
        motion,
    })
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    temperature: f32,
    stream: bool,
    /// 不写就不序列化 —— 见文件头:`thinking` 是智谱专有的,别家可能直接 400。
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Thinking>,
}

/// 智谱那套「深度思考」开关。只在配置里显式写了才发。
#[derive(Debug, Serialize)]
struct Thinking {
    #[serde(rename = "type")]
    kind: &'static str,
}

impl Thinking {
    fn new(enabled: bool) -> Self {
        Self {
            kind: if enabled { "enabled" } else { "disabled" },
        }
    }
}

#[derive(Debug, Serialize)]
struct Message {
    role: &'static str,
    content: Content,
}

/// 消息正文。OpenAI 那套里**同一个字段有两种形状**:
/// - 纯文本:`"content": "…"`;
/// - 带图:`"content": [{"type":"text",…},{"type":"image_url","image_url":{"url":"data:…"}}]`。
///
/// 做成 `untagged` 枚举,而不是"一律发数组":对话那条路一直在跑的是纯文本形状,而**不是每个模型
/// 都吃数组**(硅基流动那类兼容实现里就有不收的)。多出来的那点复杂度只花在看图这一条路上。
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Parts(Vec<Part>),
}

/// 多模态正文里的一小块。
///
/// `rename_all` 把 `ImageUrl` 写成 `image_url`(服务商要的就是这个字面量),
/// **别照着 Rust 的命名习惯想当然** —— 写成 `imageUrl` 只会拿到一句"参数不对"。
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Part {
    Text { text: String },
    ImageUrl { image_url: UrlBox },
}

#[derive(Debug, Serialize)]
struct UrlBox {
    url: String,
}

impl Message {
    fn system(content: String) -> Self {
        Self {
            role: "system",
            content: Content::Text(content),
        }
    }

    fn user(content: String) -> Self {
        Self {
            role: "user",
            content: Content::Text(content),
        }
    }

    fn assistant(content: String) -> Self {
        Self {
            role: "assistant",
            content: Content::Text(content),
        }
    }

    /// 一段话 + 一张图。图用 base64 内联(见 `image_data_url`)—— 本地图没有公网地址可给。
    fn user_with_image(content: String, data_url: String) -> Self {
        Self {
            role: "user",
            content: Content::Parts(vec![
                Part::Text { text: content },
                Part::ImageUrl {
                    image_url: UrlBox { url: data_url },
                },
            ]),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: String,
}

/// 图生视频的请求体。
///
/// `image_url` 是那张图(base64 内联,见 `image_data_url`),**也可以是两张**:智谱的
/// `cogvideox-3` 支持"首尾帧"(`image_url: [首帧, 尾帧]`)。但**产品路径只发单图**:
/// 首尾同图会把运动压成近乎静帧(§11.31),循环接缝改由播放器淡回第一帧。
///
/// `quality` / `watermark_enabled` / `duration` / `with_audio` 是显式写出来的参数:
/// - `quality: "quality"` —— 不传时服务商默认 `speed`;壁纸要画面质量,不是抢几十秒;
/// - `duration: 10` —— 5 秒和 10 秒**同价**(1 元/次),而素材长一倍,头的抖动和尾的漂移都能丢;
/// - `with_audio: false` —— 壁纸不放声音,带音轨只是白白胖一圈。
#[derive(Debug, Serialize)]
struct VideoRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    image_url: VideoImage<'a>,
    /// `quality` 不传时服务商默认 `speed`(速度优先,质量稍低)。动态壁纸要的是运动和细节,
    /// 不是早几十秒出片 —— 所以显式选 `quality`。
    quality: &'a str,
    /// 用户已签去水印免责声明。和生图一样默认关;被拒时 `submit_image` 会退回 `true` 重提。
    watermark_enabled: bool,
    /// 要出多大的片子(清单里的一档,见 `VIDEO_SIZES`)。**不能空着** —— 服务端自己推的那条对
    /// 某些比例是必死,理由写在 `VIDEO_SIZES` 上。
    size: &'a str,
    duration: u32,
    with_audio: bool,
}

/// `image_url` 的两种形状:一张图(字符串),或者首尾帧两张(数组)。
///
/// 用 `untagged` 而不是"永远发数组":单图那条路是**实测跑通的形状**(见
/// `DEFAULT_VIDEO_MODEL` 的记录),没理由为了新功能去动它。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(untagged)]
enum VideoImage<'a> {
    One(&'a str),
    Pair([&'a str; 2]),
}

/// 视频输出模式。文档写明默认是 `speed`;动态壁纸要的是画面质量和动作细节,
/// 显式选 `quality`。这和"1 元/次"同价,只是出片更慢。
const VIDEO_QUALITY: &str = "quality";

/// 一条片子的时长(秒)。文档里**只支持 5 和 10**,而且两者同价 —— 那就没有理由选短的。
const VIDEO_DURATION_SECS: u32 = 10;

/// 图生视频接口**认的那几档尺寸** —— 服务商给的清单(接口文档里就是这七个枚举),不是算出来的。
///
/// 它现在有两个用处:
/// 1. 报给接口的 `size`(不在清单里的值会被拒);
/// 2. 需要给条件图换比例时,从这里挑一档(`video_image_fix`)。
///
/// **2026-10-17 更正:上一轮把病因认错了,记在这里免得再走一遍。**
///
/// 现象是真的:两张 `1920×1088` 的图(二次元少女 / 森林小径)提交后都是 `task_status: FAIL`,
/// 正文只有一个**空的 assistant 消息**;而同一天 `1808×1136` 的图出片正常、成片 `1718×1080`
/// —— 正好是"短边压 1080、长边按原图比例、**向下取整**"这条默认路推出来的数
/// (`1080 × 1808/1136 = 1718.87 → 1718`),而 `1920×1088` 会推成 `1905`(奇数)。
/// 据此推断"奇数长边编不出来 → FAIL",并先后改了两版:显式报 `size`,再把图裁成 16:9。
///
/// 两版都没能救活那张图:**裁成 `1920×1080` 之后照样 FAIL**,而且是**熬了 10 分钟**才失败
/// (成功那次只用 10.4 秒)。同一时段拿另一个视频模型探,提交直接被
/// `429 {"error":{"code":"1305","message":"该模型当前访问量过大，请您稍后再试"}}` 挡回来。
/// 所以现在的结论是:**那几次 FAIL 是服务商侧的排队/过载**,和请求体无关。
///
/// 那这两条改动为什么还留着:
/// - 显式报一档 `size`,比留空让服务端自己推**可控**(至少出片尺寸是我们要的那一档);
/// - "推出来是奇数就裁一刀"是**便宜的保险**:H.264 的宽高确实都得是偶数,而裁一刀只丢几行像素,
///   比值本来就安全的图一分钱不花(原样发、不重编码)。
/// 但**别再把它们当成病根** —— 病根在对面。
const VIDEO_SIZES: [(u32, u32); 7] = [
    (1280, 720),
    (720, 1280),
    (1024, 1024),
    (1920, 1080),
    (1080, 1920),
    (2048, 1080),
    (3840, 2160),
];

/// 从上面那七档里挑一档最贴这块屏幕的。
///
/// 规则就三条,顺序即优先级:
/// 1. **屏幕本身就在清单里**(1920×1080 的屏、1080×1920 的竖屏、4K 屏)—— 直接用它,
///    一个像素都不用放大,这也是"屏幕多大就要多大"最理想的那种落点;
/// 2. 否则在**横竖方向一致**的档位里挑**比例最接近**的(比值的对数,2 倍和 0.5 倍算一样远);
/// 3. 比例一样近时:优先"面积不超过屏幕"的,其中取**大的**(贴得近);一档都装不下屏幕时,
///    取**小的**(放大得最少)。
///
/// 为什么第 2 步要先按横竖过滤:800×600 这种 4:3 的屏,`1280x720` 和 `720x1280` 的比例
/// 距离是**相等**的 —— 不拦一下就会给一块横屏塞一档竖屏尺寸。
pub fn video_size_for(screen: (u32, u32)) -> (u32, u32) {
    let screen = (screen.0.max(1), screen.1.max(1));
    if let Some(hit) = VIDEO_SIZES
        .iter()
        .find(|(width, height)| (*width, *height) == screen)
    {
        return *hit;
    }
    let want = (screen.0 as f32 / screen.1 as f32).ln();
    let screen_area = screen.0 as i64 * screen.1 as i64;
    // (比例距离取到千分之一:小的浮点差不会让两档"看起来一样近"却分出胜负)
    let mut best: Option<((i64, u8, i64), (u32, u32))> = None;
    for (width, height) in VIDEO_SIZES {
        // 横屏的屏不塞竖屏尺寸(竖屏反之)
        if (width >= height) != (screen.0 >= screen.1) {
            continue;
        }
        let distance = (((width as f32 / height as f32).ln() - want).abs() * 1000.0).round() as i64;
        let area = width as i64 * height as i64;
        let fits = area <= screen_area;
        let key = (distance, if fits { 0 } else { 1 }, if fits { -area } else { area });
        if best.as_ref().map(|(current, _)| key < *current).unwrap_or(true) {
            best = Some((key, (width, height)));
        }
    }
    best.map(|(_, size)| size).unwrap_or(VIDEO_SIZES[3])
}

/// 清单里的一档 → 报给接口的那个字符串(`"1920x1080"`,小写 x,没有空格)。
fn size_field(size: (u32, u32)) -> String {
    format!("{}x{}", size.0, size.1)
}
/// 服务商**自己**会推出来的尺寸:短边压到 1080,长边按图片比例走,而且**向下取整**。
///
/// 这个函数是 `VIDEO_SIZES` 上那段账的"计算器",也是 `video_image_fix` 的判据:
/// 2026-10-16 连挂三次的 `1920×1088` 推出来就是 `(1905, 1080)` —— 长边奇数,编不出来。
///
/// 别把它当成"我们想要的尺寸":我们要的那一档在 `video_size_for` 里。
pub fn derived_video_size(image: (u32, u32)) -> (u32, u32) {
    let (width, height) = (image.0.max(1), image.1.max(1));
    // 短边贴 1080、长边按比例;整数除法就是向下取整(和实测的 1718 / 1905 对得上)
    let long = |short: u32, long: u32| -> u32 {
        (1080u64 * long as u64 / short as u64).min(u32::MAX as u64) as u32
    };
    if width >= height {
        (long(height, width), 1080)
    } else {
        (1080, long(width, height))
    }
}

/// 提交前这张图**要不要先裁一刀**:要就返回"裁成哪一档的比例",不用就 `None`(原样发)。
///
/// 判据只有一条:**服务商自己推出来的长边必须是偶数** —— H.264 的宽高都得是偶数,奇数必然编不出来。
/// (2026-10-17 的实测说明这并不是那几次 FAIL 的病根,见 `VIDEO_SIZES`;这里图的是"便宜且无害",
/// 不是"救命"。)
///
/// 两条路的分工是有意的:
/// - 比值本来就安全的图**原样发**。重编码一次就是画面更糊一次,而这是**条件图** ——
///   糊了整段视频都跟着糊(同一笔账见 `image_data_url`);
/// - 不安全才裁。裁的目标是"离这张图比例最近的那一档官方尺寸":它推出来的长边必然落在
///   偶数上,而且**只丢边、不重采样**(`1920×1088` → `1920×1080`,上下各 4 行)。
///
/// 喂给 `video_size_for` 的是**图片自己的宽高**(不是屏幕):这里要保的是画面,不是屏。
pub fn video_image_fix(image: (u32, u32)) -> Option<(u32, u32)> {
    let derived = derived_video_size(image);
    if derived.0.max(derived.1) % 2 == 0 {
        return None;
    }
    Some(video_size_for(image))
}

/// 任务状态。**提交和轮询返回的是同一个形状**(实测:提交回 `PROCESSING`,轮询回 `SUCCESS` +
/// `video_result`),所以两边共用一份,不写两遍。
#[derive(Debug, Deserialize)]
struct VideoTask {
    #[serde(default)]
    id: String,
    #[serde(default)]
    task_status: String,
    #[serde(default)]
    video_result: Vec<VideoResult>,
}

#[derive(Debug, Deserialize)]
struct VideoResult {
    #[serde(default)]
    url: String,
    /// 第一帧的图。**我们不用它**:封面是自己从 mp4 里解出来的(省一次下载,
    /// 也省得依赖另一个 24 小时后就会失效的地址),留着只是因为它是响应的一部分。
    #[serde(default)]
    #[allow(dead_code)]
    cover_image_url: String,
}

/// 生图请求体。OpenAI 兼容的生图接口都是这个形状(`size` 是 `"宽x高"` 的字符串)。
#[derive(Debug, Serialize)]
struct ImageRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    size: String,
    /// `hd` / `standard`。**必须显式带上**:不写这个字段就是 standard,
    /// 而 standard 出图明显更糊(实测高频细节差 41%)。
    quality: &'a str,
    /// 带不带「AI生成」角标。**同样显式带上**:不写就是 `true`。
    watermark_enabled: bool,
}

/// 生图响应。只有一个字段真的有用:`data[0].url`。
#[derive(Debug, Deserialize)]
struct ImageResponse {
    #[serde(default)]
    data: Vec<ImageDatum>,
}

#[derive(Debug, Deserialize)]
struct ImageDatum {
    #[serde(default)]
    url: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 画质这个字段**不能漏**:服务商那边不写就是 `standard`,而出图明显更糊
    /// (实测同一句提示词、同一个尺寸,高频细节差 41%)。这条盯的是"请求体里真有它"。
    #[test]
    fn 生图请求体里带上画质() {
        let body = serde_json::to_string(&ImageRequest {
            model: DEFAULT_IMAGE_MODEL,
            prompt: "雪山湖的清晨",
            size: "1728x960".to_string(),
            quality: DEFAULT_IMAGE_QUALITY,
            watermark_enabled: DEFAULT_IMAGE_WATERMARK,
        })
        .unwrap();
        assert!(
            body.contains("\"quality\":\"hd\""),
            "请求体里没有画质字段:{body}"
        );
        // 别的值服务商会 400,所以默认值只能是这两个中的一个
        assert!(matches!(DEFAULT_IMAGE_QUALITY, "hd" | "standard"));
    }

    /// 水印这个字段同样**不能漏**:不写就是 `true`,服务商会往右下角盖一枚「AI生成」。
    /// 而且它必须是**显式的布尔**:`false` 和"没这个字段"在服务商那边是两件事。
    #[test]
    fn 生图请求体里明确写了要不要水印() {
        let body = serde_json::to_string(&ImageRequest {
            model: DEFAULT_IMAGE_MODEL,
            prompt: "雪山湖的清晨",
            size: "1920x1088".to_string(),
            quality: DEFAULT_IMAGE_QUALITY,
            watermark_enabled: DEFAULT_IMAGE_WATERMARK,
        })
        .unwrap();
        assert!(
            body.contains("\"watermark_enabled\":false"),
            "默认这一档应该是「不带水印」,而请求体里是:{body}"
        );
        assert!(!DEFAULT_IMAGE_WATERMARK, "默认值变了,记得同步这条测试");

        // 反过来也得写出来(用户显式要带水印时)
        let body = serde_json::to_string(&ImageRequest {
            model: DEFAULT_IMAGE_MODEL,
            prompt: "雪山湖的清晨",
            size: "1920x1088".to_string(),
            quality: DEFAULT_IMAGE_QUALITY,
            watermark_enabled: true,
        })
        .unwrap();
        assert!(body.contains("\"watermark_enabled\":true"), "{body}");
    }

    /// 兜底重发的那道闸:只认"报错正文提到水印"的这一种,别把别的 4xx 也吞下去重发。
    #[test]
    fn 只在水印被拒时才兜底重发() {
        // 认得出的:提到 watermark / 水印
        assert!(is_watermark_refusal(
            400,
            r#"{"error":{"code":"1214","message":"watermark_enabled=false 需要先签署免责声明"}}"#
        ));
        assert!(is_watermark_refusal(403, "该账号未开通去水印权限"));
        // 认不出的:别的 400(参数不合法 / 欠费 / 模型不存在)一律不重发
        assert!(!is_watermark_refusal(400, r#"{"error":{"message":"size 不合法"}}"#));
        assert!(!is_watermark_refusal(401, r#"{"error":{"message":"invalid api key"}}"#));
        // 200 更不该被当成"被拒" —— 那会白画第二张
        assert!(!is_watermark_refusal(200, "ok"));
        // 5xx 也不重发:服务端自己出问题,重发只是再等一轮 45 秒
        assert!(!is_watermark_refusal(500, "watermark service unavailable"));
    }

    /// 环境变量里的布尔只认常见写法,认不出的**往下走**而不是替用户挑一个。
    #[test]
    fn 环境变量那个布尔认不出就不猜() {
        for raw in ["1", "true", "TRUE", "on", "Yes", " true "] {
            assert_eq!(parse_bool(raw), Some(true), "{raw} 应该算真");
        }
        for raw in ["0", "false", "OFF", "no", "False"] {
            assert_eq!(parse_bool(raw), Some(false), "{raw} 应该算假");
        }
        for raw in ["", "0off", "也许", "nope", "2"] {
            assert_eq!(parse_bool(raw), None, "{raw} 不该被当成一个确定的值");
        }
    }

    /// 规则本身是从接口的 4xx 正文抄下来的(见 `SIZE_MIN` 那段),不是猜的 —— 先把它钉住。
    /// 下面那些"折出来的尺寸合法"的断言全靠这一条才有意义。
    #[test]
    fn 尺寸规则就是那三条() {
        // 反面例子得钉住,否则正面断言可能只是"恒真":
        assert!(!size_is_legal(1600, 900), "900 不是 32 的倍数");
        assert!(!size_is_legal(2048, 2112), "总像素超了 2²² 也算不合法");
        assert!(!size_is_legal(4096, 1024), "4096 超过 2880 的上界");
        assert!(!size_is_legal(480, 512), "480 低于 512 的下界");
        // 正面:1920×1088 正是这台机器的屏折出来的那个,2560×1600 是"上一轮最亏"的那一个
        assert!(size_is_legal(1920, 1088));
        assert!(size_is_legal(2560, 1600));
        // 顺手钉一句:"16 的倍数"那套不能再回来。库里那批 1808×1136 就是这么算出来的,
        // 到 glm-image 这边 1808/32 = 56.5,会被 400 直接拒掉。
        assert_ne!(1808 % SIZE_STEP, 0);
    }

    /// 挑尺寸:**屏幕多大就要多大**,只在接口不允许时才动手。
    #[test]
    fn 屏幕多大就要多大() {
        // 这台机器的屏 1920×1080:1080 不是 32 的倍数,往**上**补到 1088(不是裁到 1056)
        assert_eq!(fit_size(1920, 1080), (1920, 1088));
        // 2560×1600 **原样发得出去**(2560/32 = 80、1600/32 = 50、4.096M ≤ 4.19M)。
        // 这正是"从七档推荐里挑"最亏的那一条:那会儿只能要 1568×1056,上屏得放大 1.63 倍
        assert_eq!(fit_size(2560, 1600), (2560, 1600));
        // 竖屏 / 1:1 / 16:9 都走同一条路,没有谁被"就近取整"到某个推荐档
        assert_eq!(fit_size(1080, 1920), (1088, 1920));
        assert_eq!(fit_size(1280, 1280), (1280, 1280));
        assert_eq!(fit_size(1280, 720), (1280, 736));
        assert_eq!(fit_size(1600, 1200), (1600, 1216));
    }

    /// 撞天花板时**只能等比缩**,不许夹单个维度(夹了就毁画面比例)。
    #[test]
    fn 撞天花板时等比缩而不是夹一个维度() {
        // 4K:两条线一起撞。夹长边会得到 2880×2176(1.32 —— 16:9 被压成 4:3 的宽);
        // 现在是 2720×1536,还留在 16:9 上,总像素也正好卡进 2²²
        assert_eq!(fit_size(3840, 2160), (2720, 1536));
        // 21:9 只撞长边那一条。高度这里值得看一眼:等比缩到长边 2880 时理想高度正好是 **1200**,
        // 而 1200/32 = 37.5 —— 它压根不是 32 的倍数,只能落到 1184 或 1216。
        // 取 1216 不是"四舍五入碰巧舍上去了":2880/1184 = 2.43(离 2.39 差 1.8%),
        // 2880/1216 = 2.37(差 0.9%),1216 那一侧才是贴着原比例的那个。
        assert_eq!(fit_size(3440, 1440), (2880, 1216));
        // 5K 和 4K 是同一笔账(长边先缩到 2880,再压像素)
        assert_eq!(fit_size(5120, 2880), (2720, 1536));
    }

    /// 无论屏长什么样,出来的一定**合法**、而且**比例没被弄歪**。
    ///
    /// 这条是 `fit_size` 收尾那记 `debug_assert` 的替身 —— release 构建里那记断言不编译,
    /// 而尺寸算错的代价是"用户等一分钟、什么也没拿到"。
    #[test]
    fn 屏幕尺寸总能折成合法请求尺寸() {
        let screens = [
            (1920u32, 1080u32),
            (2560, 1600),
            (2560, 1440),
            (3840, 2160),
            (5120, 2880),
            (3440, 1440),
            (1680, 1050),
            (1366, 768),
            (1280, 720),
            (800, 600),
            (640, 480),
            (1024, 1280),
            (1080, 1920),
            (3840, 1080),
            (1080, 3840),
            (300, 200),
            (1, 1),
        ];
        for (screen_width, screen_height) in screens {
            let (width, height) = fit_size(screen_width, screen_height);
            assert!(
                size_is_legal(width, height),
                "{screen_width}×{screen_height} 折成了不合法的 {width}×{height}"
            );
            // 比例:32 的粒度决定了两边各会差半档,所以这里用**相对**判据 ——
            // 比例漂移不许过 3%(实测最差的是 3840×1080 那种 32:9:2.67%,粒度到头了)。
            // 夹单个维度会立刻炸:3840×1080 夹成 2880×1080 漂 24%。
            let screen_ratio = screen_width as f64 / screen_height as f64;
            let request_ratio = width as f64 / height as f64;
            let drift = (request_ratio / screen_ratio).ln().abs();
            assert!(
                drift < 0.03,
                "{screen_width}×{screen_height}({screen_ratio:.3})折成了 {width}×{height}({request_ratio:.3}),漂了 {:.1}%",
                drift * 100.0
            );
        }
    }

    /// 小模型最常见的走样有两处:把 `motion` 整块写成一句字符串,以及干脆不写 `motion`。
    /// 两种都必须在本地修好,不能让整次调用白费。
    #[test]
    fn 生图计划能修掉走样的动效字段() {
        let text = r#"{
            "reply": "给你画个雪夜",
            "title": "雪夜暖灯",
            "image_prompt": "雪夜里的木屋,窗内暖黄灯光,冷蓝雪地,远景松林,高清数字插画",
            "motion": "snow"
        }"#;
        let plan = parse_photo_reply(text).expect("该能解析");
        assert_eq!(plan.title, "雪夜暖灯");
        assert_eq!(plan.motion.kind, MotionKind::Snow);
        assert_eq!(plan.motion.zoom, MotionRecipe::default().zoom);

        // 缺 motion:回默认,而不是报错
        let plan = parse_photo_reply(r#"{ "image_prompt": "海面日出" }"#).unwrap();
        assert_eq!(plan.motion.kind, MotionKind::Still);
        assert_eq!(plan.title, "海面日出");
        assert!(plan.reply.contains("海面日出"), "没写回复就用标题兜一句");

        // 没给提示词:**这一条必须失败**(没提示词就没有图可画),交给重试
        assert!(parse_photo_reply(r#"{ "reply": "好" }"#).is_err());
    }

    /// 提示词里那份动效清单是手写的字面量,枚举改了它不会自己跟着改 —— 这个测试就是那道闸。
    #[test]
    fn 提示词里的动效清单和枚举一致() {
        for kind in MotionKind::ALL {
            assert!(
                PHOTO_SYSTEM_PROMPT.contains(kind.id()),
                "系统提示词里没有动效 {}",
                kind.id()
            );
        }
    }

    /// 看图提示词里那十个位置,必须和 `mcwp_spec::AREA_NAMES` **同序**。
    ///
    /// 这条是跨 crate 的约定:模型照着提示词列的清单回答("左上"),渲染器照着
    /// `AREA_NAMES` 的下标去画。两边一错位,左上角的灯就被画到右下角 —— 而且
    /// 画面看起来"确实在动",不会报错,只能靠这条测试盯着。
    #[test]
    fn 提示词里的位置清单和编号同序() {
        let mut cursor = 0usize;
        for (index, name) in mcwp_spec::AREA_NAMES.iter().enumerate() {
            let at = VISION_SYSTEM_PROMPT[cursor..]
                .find(name)
                .unwrap_or_else(|| panic!("提示词里找不到位置「{name}」"));
            let abs = cursor + at;
            assert!(
                abs >= cursor,
                "位置「{name}」(编号 {index})在提示词里排在「{}」前面了",
                mcwp_spec::AREA_NAMES[index - 1]
            );
            cursor = abs + name.len();
        }
    }

    /// 看图提示词必须明确排除"光影在动"这类伪运动,并让前景实体排在前面。
    ///
    /// 用户报的"生成的动图只是光影变化"就是从这条缝里出来的:模型会挑最安全、最容易
    /// 描述的光线明暗,而不是草和树叶。规则一旦被删,症状会原样回来。
    #[test]
    fn 看图提示词禁止光影伪运动并优先前景() {
        for forbidden in ["太阳", "夕阳", "光线", "光晕", "阴影", "曝光", "亮度"] {
            assert!(
                VISION_SYSTEM_PROMPT.contains(forbidden),
                "提示词里没有排除「{forbidden}」"
            );
        }
        assert!(VISION_SYSTEM_PROMPT.contains("元素顺序前景优先"));
        assert!(VISION_SYSTEM_PROMPT.contains("至少 2 个实体"));
    }

    /// 生图响应只认 `data[0].url`,但它真实的形状比这花哨得多,多一层嵌套不能把解析搞崩。
    #[test]
    fn 生图响应能挑出图片地址() {
        let raw = r#"{"id":"task","created":1760000000,"data":[{"url":"https://example.com/a.png"}]}"#;
        let parsed: ImageResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.data.len(), 1);
        assert_eq!(parsed.data[0].url, "https://example.com/a.png");
        // 没有 data 字段时是空数组,不是解析失败 —— 上层据此报"没给图片地址"
        let empty: ImageResponse = serde_json::from_str(r#"{"error":"quota"}"#).unwrap();
        assert!(empty.data.is_empty());
    }

    /// 降级提示词要把三条底线兜住(不要文字 / 留白 / 画风),不能只是用户原话。
    #[test]
    fn 降级提示词兜住了底线() {
        let prompt = fallback_image_prompt("  我想要个安静点的桌面  ");
        assert!(prompt.starts_with("我想要个安静点的桌面"), "开头就是用户原话");
        assert!(prompt.contains("无文字"));
        assert!(prompt.contains("留白"));
    }

    /// base64 是手写的(见 `base64` 的说明),拿 RFC 4648 那几条已知向量钉住 ——
    /// 三个字节 → 四个字符、两个字节补一个 `=`、一个字节补两个 `=` 这三种情况都要覆盖,
    /// 因为"补齐"这一支写错的概率最高,而错了之后服务商只会回一句莫名其妙的 400。
    #[test]
    fn base64_编码和已知向量一致() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        // 二进制里的高位字节(0x80 / 0xFF)不能被当成符号位处理
        assert_eq!(base64(&[0x80, 0xFF, 0x00]), "gP8A");
    }

    /// 内联那张图的 MIME 得按**文件头**认:认错了服务商那边是"图片解析失败",
    /// 而报错信息里根本不会提到我们给错了类型。
    #[test]
    fn 内联图片按文件头认类型() {
        assert!(image_data_url(&[0x89, b'P', b'N', b'G']).starts_with("data:image/png;base64,"));
        assert!(image_data_url(&[0xFF, 0xD8, 0xFF, 0xE0]).starts_with("data:image/jpeg;base64,"));
    }

    /// 轮询那条路返回的形状(实测样本),三种状态都要认出来:
    /// 跑着 / 出来了 / 失败了。**注意它可能配着 HTTP 400 回来** —— 所以状态只从 body 里读。
    #[test]
    fn 视频任务状态能认出来() {
        let running: VideoTask =
            serde_json::from_str(r#"{"id":"task-1","task_status":"PROCESSING"}"#).unwrap();
        assert_eq!(running.task_status, "PROCESSING");
        assert!(running.video_result.is_empty());

        let done: VideoTask = serde_json::from_str(
            r#"{"id":"task-1","task_status":"SUCCESS","video_result":[
                 {"url":"https://example.com/a.mp4","cover_image_url":"https://example.com/a.png"}]}"#,
        )
        .unwrap();
        assert_eq!(done.video_result.len(), 1);
        assert_eq!(done.video_result[0].url, "https://example.com/a.mp4");
        assert_eq!(done.video_result[0].cover_image_url, "https://example.com/a.png");

        let failed: VideoTask =
            serde_json::from_str(r#"{"id":"task-1","task_status":"FAIL"}"#).unwrap();
        assert_eq!(failed.task_status, "FAIL");
    }

    /// 模型那句"怎么动"要能被收干净:围栏、"提示词:"前缀、引号、后面跟着的解释都得去掉。
    /// 这些字多留一个,就会变成视频里莫名其妙的东西。
    #[test]
    fn 动效提示词会被收干净() {
        assert_eq!(
            clean_motion_reply("```\n镜头缓慢推近,云在飘\n```"),
            "镜头缓慢推近,云在飘"
        );
        assert_eq!(
            clean_motion_reply("提示词:镜头缓慢推近,水面泛起波纹"),
            "镜头缓慢推近,水面泛起波纹"
        );
        assert_eq!(
            clean_motion_reply("“镜头缓慢拉远,雾气流动”"),
            "镜头缓慢拉远,雾气流动"
        );
        // 只有围栏、没有正文时返回空串,由调用方回退到通用运镜
        assert!(clean_motion_reply("```json\n```").is_empty());
        // 太长会截断(`truncate` 会在尾巴上加一个省略号,所以是 max + 1)
        let long = clean_motion_reply(&"推".repeat(600));
        assert!(long.ends_with('…'));
        assert!(long.chars().count() <= MAX_MOTION_PROMPT_CHARS + 1);
    }

    /// 免费模型爱写 "bends gracefully / drift lazily" 这种幅度很小的句子。
    /// 只换副词不够,要把动作短语本身换强,否则 quality 档会把镜头锁住后几乎不动。
    #[test]
    fn 弱动作短语会被换成看得出幅度的写法() {
        assert_eq!(
            strengthen_motion("Grass bends gracefully in the wind."),
            "Grass bends deeply in the wind."
        );
        assert_eq!(
            strengthen_motion("Clouds drift lazily across the sky."),
            "Clouds stream steadily across the sky."
        );
        assert_eq!(
            strengthen_motion("Leaves sway gently in the breeze."),
            "Leaves sway strongly in the breeze."
        );
    }

    /// 视觉模型那份 JSON → 元素级提示词。
    ///
    /// 这条路的产物**直接就是发给图生视频那句话**,所以这里钉的是三件最容易写丢的事:
    /// 每个元素都进得去、位置在元素前面(中文里"左中灯光在闪"才通顺)、末尾那句闭环要求还在。
    #[test]
    fn 元素级方案拼成一句提示词() {
        let plan = MotionPlan {
            camera: "固定机位".to_string(),
            elements: vec![
                MotionElement {
                    kind: "灯光".to_string(),
                    area: "左中".to_string(),
                    action: "窗口逐个亮灭闪烁".to_string(),
                    sentence: String::new(),
                },
                MotionElement {
                    kind: "车流".to_string(),
                    area: "下中".to_string(),
                    action: "沿道路从右往左流动".to_string(),
                    sentence: String::new(),
                },
            ],
            caption: String::new(),
            scene: String::new(),
        };
        let prompt = video_prompt(&plan);
        assert!(prompt.contains("镜头固定机位"), "{prompt}");
        assert!(prompt.contains("左中灯光窗口逐个亮灭闪烁"), "{prompt}");
        assert!(prompt.contains("下中车流沿道路从右往左流动"), "{prompt}");
        // 闭环要求**不能少**:循环播放能不能接上,一半靠这句(见 §11.22)
        assert!(prompt.contains("结尾回到起始画面"), "{prompt}");
    }

    /// 方案是空的(看图失败 / 没配视觉模型)→ 必须退回那句通用运镜,而不是发一句空提示词。
    /// 元素再多,收尾那几句也不能被截掉。
    ///
    /// 这条是**按实测补的**:截断砍的是尾巴,而元素一多、最先没的就是"结尾回到起始画面"。
    /// 少一个元素只是少动一处;闭环那句没了,循环点上那一记跳就回来了(见 §11.22)。
    #[test]
    fn 元素再多也留得住闭环那句() {
        let plan = MotionPlan {
            camera: "固定机位".to_string(),
            // 故意写超长:五条各 200 字,合起来远超 512 的上限,好把截断这一路逼出来
            elements: (0..MotionPlan::MAX_ELEMENTS)
                .map(|index| MotionElement {
                    kind: format!("元素{index}"),
                    area: "正中".to_string(),
                    action: "缓慢地来回摆动".repeat(20),
                    sentence: String::new(),
                })
                .collect(),
            caption: String::new(),
            scene: String::new(),
        };
        let prompt = video_prompt(&plan);
        assert!(
            prompt.chars().count() <= MAX_VIDEO_PROMPT_CHARS,
            "还是超了:{}",
            prompt.chars().count()
        );
        assert!(prompt.contains("结尾回到起始画面"), "{prompt}");
        assert!(prompt.ends_with("结尾回到起始画面。"), "{prompt}");
    }

    /// 方案是空的(看图失败 / 没配视觉模型)→ 必须退回那句通用运镜,而不是发一句空提示词。
    #[test]
    fn 空方案退回通用运镜() {
        assert_eq!(
            video_prompt(&MotionPlan::default()),
            fallback_motion_prompt()
        );
        // 只有镜头、没有元素也算空:视频模型需要的是"哪儿在动",不是"机位怎么走"
        let camera_only = MotionPlan {
            camera: "缓慢推近".to_string(),
            elements: Vec::new(),
            caption: String::new(),
            scene: String::new(),
        };
        assert_eq!(video_prompt(&camera_only), fallback_motion_prompt());
    }

    /// 新方案:scene + 英文 elements 在本地拼成官方要的英文长描述。
    ///
    /// 钉住三件事:相机在最前面、每个元素的动作都进得去、闭环句没被挤掉。
    #[test]
    fn scene_和元素拼成英文长描述() {
        let plan = MotionPlan {
            camera: "固定机位".to_string(),
            scene: "A quiet forest path at dusk with warm sunlight through tall trees".to_string(),
            elements: vec![
                MotionElement {
                    kind: "tree leaves".to_string(),
                    area: "左上".to_string(),
                    action: "sway back and forth in a light breeze".to_string(),
                    sentence: String::new(),
                },
                MotionElement {
                    kind: "grass".to_string(),
                    area: "右下".to_string(),
                    action: "rustle and bend along the trail".to_string(),
                    sentence: String::new(),
                },
                MotionElement {
                    kind: "thin clouds".to_string(),
                    area: "右上".to_string(),
                    action: "drift slowly to the left".to_string(),
                    sentence: String::new(),
                },
            ],
            caption: String::new(),
        };
        let prompt = video_prompt(&plan);
        assert!(
            prompt.starts_with("A quiet forest path through tall trees"),
            "{prompt}"
        );
        assert!(
            prompt.contains("The camera remains locked off."),
            "{prompt}"
        );
        assert!(
            prompt.contains("In the upper left, tree leaves sway back and forth"),
            "{prompt}"
        );
        assert!(
            prompt.contains("In the lower right, grass rustle and bend"),
            "{prompt}"
        );
        assert!(
            prompt.contains("In the upper right, thin clouds stream steadily"),
            "{prompt}"
        );
        assert!(prompt.ends_with("no camera movement, zoom, pan or orbit, and exposure and color stay fixed."), "{prompt}");
        assert!(
            prompt.chars().count() <= MAX_VIDEO_PROMPT_CHARS,
            "{}",
            prompt.chars().count()
        );
    }

    /// 视频模型对前几句最敏感。视觉模型常把 clouds 写在第一句,这里必须
    /// 在本地改成前景草/蕨先出现,否则出片又会先动天空和光影。
    #[test]
    fn 前景元素排在天空前面() {
        let plan = MotionPlan {
            camera: "固定机位".to_string(),
            scene: "A forest path".to_string(),
            elements: vec![
                MotionElement {
                    kind: "clouds".to_string(),
                    area: "sky".to_string(),
                    action: "drift".to_string(),
                    sentence: "Clouds drift across the sky.".to_string(),
                },
                MotionElement {
                    kind: "tree leaves".to_string(),
                    area: "midground".to_string(),
                    action: "sway".to_string(),
                    sentence: "Tree leaves sway back and forth.".to_string(),
                },
                MotionElement {
                    kind: "grass".to_string(),
                    area: "foreground".to_string(),
                    action: "bend".to_string(),
                    sentence: "Grass bends in visible waves.".to_string(),
                },
            ],
            caption: String::new(),
        };
        let prompt = video_prompt(&plan);
        let grass = prompt.find("Grass bends").expect("grass should be present");
        let leaves = prompt.find("Tree leaves sway").expect("leaves should be present");
        let clouds = prompt.find("Clouds drift").expect("clouds should be present");
        assert!(grass < leaves && leaves < clouds, "{prompt}");
        assert!(prompt.ends_with("no camera movement, zoom, pan or orbit, and exposure and color stay fixed."), "{prompt}");
    }

    /// 旧方案里的 `Sunset / The sun sets...` 不能再发给视频模型。
    /// 实体保留,光影片段丢掉;全丢光时退回那条写实体运动的通用提示词。
    #[test]
    fn 光影片段会被丢掉() {
        let plan = MotionPlan {
            camera: "固定机位".to_string(),
            scene: "A forest path".to_string(),
            elements: vec![
                MotionElement {
                    kind: "Sunset".to_string(),
                    area: "sky".to_string(),
                    action: "rise".to_string(),
                    sentence: "The sun sets on the horizon.".to_string(),
                },
                MotionElement {
                    kind: "grass".to_string(),
                    area: "foreground".to_string(),
                    action: "bend".to_string(),
                    sentence: "Grass bends back and forth in the wind.".to_string(),
                },
            ],
            caption: String::new(),
        };
        let prompt = video_prompt(&plan);
        assert!(!prompt.contains("sun sets"), "{prompt}");
        assert!(prompt.contains("Grass bends back and forth"), "{prompt}");

        let light_only = MotionPlan {
            camera: "固定机位".to_string(),
            scene: "A room".to_string(),
            elements: vec![MotionElement {
                kind: "Sunlight".to_string(),
                area: "全图".to_string(),
                action: "fade".to_string(),
                sentence: "Sunlight fades across the room.".to_string(),
            }],
            caption: String::new(),
        };
        assert_eq!(video_prompt(&light_only), fallback_motion_prompt());
    }

    /// 免费模型会把 "English scene anchor" 抄进正文,也会带 `at dusk` 这类时间词。
    /// 本地必须清掉,否则视频模型又会去动太阳和整体光线。
    #[test]
    fn 场景锚点会清掉英文前缀和光线时间词() {
        assert_eq!(clean_scene_anchor("English forest path at dusk"), "Forest path");
        assert_eq!(
            clean_scene_anchor("a quiet lake at sunset with warm sunlight"),
            "A quiet lake"
        );
        assert_eq!(clean_scene_anchor("森林小径"), "森林小径");
    }

    /// 元素带完整英文句时优先用完整句(主谓一致),不再本地拼 "位置 + 元素 + 动作"。
    #[test]
    fn 完整句优先于本地拼装() {
        let plan = MotionPlan {
            camera: "固定机位".to_string(),
            scene: "A quiet forest path at sunset with warm light through tall trees".to_string(),
            elements: vec![MotionElement {
                kind: "thin clouds".to_string(),
                area: "右上".to_string(),
                action: "drift slowly left".to_string(),
                sentence: "Thin clouds drift slowly to the left across the orange sky.".to_string(),
            }],
            caption: String::new(),
        };
        let prompt = video_prompt(&plan);
        assert!(
            prompt.contains("Thin clouds stream steadily to the left"),
            "{prompt}"
        );
        assert!(
            !prompt.contains("In the upper right, thin clouds"),
            "{prompt}"
        );
        assert!(
            prompt.contains("The camera remains locked off."),
            "{prompt}"
        );
        assert!(prompt.ends_with("no camera movement, zoom, pan or orbit, and exposure and color stay fixed."), "{prompt}");
    }

    /// caption 是主路:有它就直接发,不再退回"镜头 + 元素"的短清单。
    #[test]
    fn caption_优先于元素拼接() {
        let plan = MotionPlan {
            camera: "缓慢推近".to_string(),
            elements: vec![MotionElement {
                kind: "灯光".to_string(),
                area: "正中".to_string(),
                action: "闪烁".to_string(),
                sentence: String::new(),
            }],
            caption: "A forest path at dusk. The camera remains locked off. Ferns sway back and forth in a light breeze, while thin clouds drift slowly across the sky. Each motion completes one full visible cycle and returns to its starting pose; no camera movement, zoom, pan or orbit, and exposure and color stay fixed.".to_string(),
            scene: String::new(),
        };
        let prompt = video_prompt(&plan);
        assert!(prompt.starts_with("A forest path at dusk."), "{prompt}");
        assert!(!prompt.contains("镜头缓慢推近"), "{prompt}");
        assert!(prompt.contains("Ferns sway back and forth"), "{prompt}");
        assert!(prompt.ends_with("no camera movement, zoom, pan or orbit, and exposure and color stay fixed."), "{prompt}");
    }

    /// caption 里没提相机 → 本地补一句固定机位;没提闭环 → 补一句闭环。
    #[test]
    fn caption_缺相机和闭环会补齐() {
        let plan = MotionPlan {
            camera: String::new(),
            elements: Vec::new(),
            caption: "Leaves rustle in the breeze and grass sways along the path.".to_string(),
            scene: String::new(),
        };
        let prompt = video_prompt(&plan);
        assert!(
            prompt.starts_with("The camera remains locked off."),
            "{prompt}"
        );
        assert!(prompt.contains("Leaves rustle"), "{prompt}");
        assert!(prompt.ends_with("no camera movement, zoom, pan or orbit, and exposure and color stay fixed."), "{prompt}");
        assert!(prompt.chars().count() <= MAX_VIDEO_PROMPT_CHARS, "{prompt}");
    }

    /// caption 超过 512 字符 → 按句子边界收口,不能拦腰砍半句。
    #[test]
    fn caption_超长按句子收口() {
        let caption = "A quiet forest path at dusk. The camera remains locked off. Ferns sway back and forth in a light breeze. Thin clouds drift slowly across the sky. Grass blades bend and spring back along both edges of the trail. Leaves flutter and rustle in the canopy above. Warm light shifts across the moss as the branches move. Distant branches bob gently against the orange horizon. A few small birds glide past above the treetops. Dust motes float through the low sunbeams. Each motion completes one full visible cycle and returns to its starting pose; no camera movement, zoom, pan or orbit, and exposure and color stay fixed. Extra sentence that should be cut off cleanly.";
        let plan = MotionPlan {
            camera: String::new(),
            elements: Vec::new(),
            caption: caption.to_string(),
            scene: String::new(),
        };
        let prompt = video_prompt(&plan);
        assert!(
            prompt.chars().count() <= MAX_VIDEO_PROMPT_CHARS,
            "{}",
            prompt.chars().count()
        );
        assert!(prompt.ends_with('.'), "{prompt}");
        assert!(!prompt.contains("Extra sentence"), "{prompt}");
    }

    /// 通用兜底也不能只剩光影:必须写清树叶/草/水面这些实体怎么动。
    #[test]
    fn 通用兜底写的是实体运动() {
        let prompt = fallback_motion_prompt();
        assert!(prompt.contains("camera remains locked off"), "{prompt}");
        assert!(prompt.contains("leaves sway"), "{prompt}");
        assert!(prompt.contains("ripple outward"), "{prompt}");
        assert!(
            prompt.chars().count() <= MAX_VIDEO_PROMPT_CHARS,
            "{}",
            prompt.chars().count()
        );
    }

    /// 只有 caption 也算"有方案";老资产只有 elements 时 `has_caption` 必须是 false,
    /// 这样 `animate_photo` 才会重新看图,而不是复用旧的两元素方案。
    #[test]
    fn 有无_caption_决定能不能复用旧方案() {
        let legacy = MotionPlan {
            camera: "固定机位".to_string(),
            elements: vec![MotionElement {
                kind: "阳光".to_string(),
                area: "全图".to_string(),
                action: "一明一暗".to_string(),
                sentence: String::new(),
            }],
            caption: String::new(),
            scene: String::new(),
        };
        assert!(!legacy.is_empty());
        assert!(!legacy.has_caption());

        let fresh = MotionPlan {
            camera: String::new(),
            elements: Vec::new(),
            caption: "Leaves sway in the breeze.".to_string(),
            scene: String::new(),
        };
        assert!(!fresh.is_empty());
        assert!(fresh.has_caption());
    }

    /// 动作里又写了一遍元素名时不重复 —— 提示词里交代过"别重复",但模型不一定听。
    /// 拼成"正中灯光灯光逐个亮灭闪烁"会让视频模型以为画面里有两处灯。
    #[test]
    fn 元素名和动作重复时只留一份() {
        let plan = MotionPlan {
            camera: "固定机位".to_string(),
            elements: vec![
                MotionElement {
                    kind: "灯光".to_string(),
                    area: "正中".to_string(),
                    action: "灯光逐个亮灭闪烁".to_string(),
                    sentence: String::new(),
                },
                // 更隐蔽的那种:重合的只有"云"一个字
                MotionElement {
                    kind: "云朵".to_string(),
                    area: "左上".to_string(),
                    action: "云向右缓慢飘移".to_string(),
                    sentence: String::new(),
                },
            ],
            caption: String::new(),
            scene: String::new(),
        };
        let prompt = video_prompt(&plan);
        assert!(prompt.contains("正中灯光逐个亮灭闪烁"), "{prompt}");
        assert!(!prompt.contains("灯光灯光"), "{prompt}");
        assert!(prompt.contains("左上云朵向右缓慢飘移"), "{prompt}");
        assert!(!prompt.contains("云朵云"), "{prompt}");
    }

    /// 元素条数有上限、空字段要丢掉 —— 视觉模型的返法很随意(偶尔漏字段、偶尔多给一倍)。
    #[test]
    fn 元素清单会收口() {
        let mut plan = MotionPlan {
            camera: "  固定机位  ".to_string(),
            elements: vec![
                MotionElement {
                    kind: " 云 ".to_string(),
                    area: "上中".to_string(),
                    action: " 向右缓慢飘移 ".to_string(),
                    sentence: String::new(),
                },
                // 没有 action 的:丢掉
                MotionElement {
                    kind: "车流".to_string(),
                    area: "下中".to_string(),
                    action: "   ".to_string(),
                    sentence: String::new(),
                },
                // 没有 kind 的:丢掉
                MotionElement {
                    kind: String::new(),
                    area: "左上".to_string(),
                    action: "闪烁".to_string(),
                    sentence: String::new(),
                },
            ],
            caption: "  A forest path at dusk. The camera remains locked off.  ".to_string(),
            scene: String::new(),
        };
        plan.normalize();
        assert_eq!(plan.camera, "固定机位");
        assert_eq!(
            plan.caption,
            "A forest path at dusk. The camera remains locked off."
        );
        assert_eq!(plan.elements.len(), 1);
        assert_eq!(plan.elements[0].kind, "云");
        assert_eq!(plan.elements[0].action, "向右缓慢飘移");

        // 超量时只留前面几个(顺序就是"动了最出效果"的优先级,不能按别的规则砍)
        let mut many = MotionPlan::default();
        for index in 0..(MotionPlan::MAX_ELEMENTS + 3) {
            many.elements.push(MotionElement {
                kind: format!("元素{index}"),
                area: "正中".to_string(),
                action: "动".to_string(),
                sentence: String::new(),
            });
        }
        many.normalize();
        assert_eq!(many.elements.len(), MotionPlan::MAX_ELEMENTS);
        assert_eq!(many.elements[0].kind, "元素0");
    }

    /// 首尾帧那两种请求形状:**单图必须是字符串、首尾帧必须是两张的数组**。
    /// 服务商认的是形状本身,写错就是一次白问(而且这条只有发出去才知道错了)。
    #[test]
    fn 图生视频请求的两种形状() {
        let one = serde_json::to_value(&VideoRequest {
            model: "cogvideox-3",
            prompt: "镜头固定",
            image_url: VideoImage::One("data:image/png;base64,AAA"),
            quality: VIDEO_QUALITY,
            watermark_enabled: false,
            size: "1920x1080",
            duration: VIDEO_DURATION_SECS,
            with_audio: false,
        })
        .unwrap();
        assert!(one["image_url"].is_string(), "{one}");
        // `size` **必须**报出去:留空时服务端按原图比例推,某些比例会推出奇数长边然后 FAIL
        // (见 `VIDEO_SIZES`)。这条钉住"它一直在请求体里",别再被谁当成可选项删掉。
        assert_eq!(one["size"], "1920x1080");
        assert_eq!(one["duration"], 10);
        assert_eq!(one["with_audio"], false);
        // `quality` 不传就是 `speed`;动态壁纸必须显式选质量优先。
        assert_eq!(one["quality"], "quality");
        // 用户已签去水印声明;默认无水印,被拒才由 `submit_image` 退回带水印。
        assert_eq!(one["watermark_enabled"], false);

        let pair = serde_json::to_value(&VideoRequest {
            model: "cogvideox-3",
            prompt: "镜头固定",
            image_url: VideoImage::Pair(["data:image/png;base64,AAA", "data:image/png;base64,AAA"]),
            quality: VIDEO_QUALITY,
            watermark_enabled: false,
            size: "1920x1080",
            duration: VIDEO_DURATION_SECS,
            with_audio: false,
        })
        .unwrap();
        let images = pair["image_url"].as_array().expect("首尾帧要发成数组");
        assert_eq!(images.len(), 2);
        assert_eq!(images[0], images[1], "首尾帧给的是同一张图");
    }

    /// 看图那条消息的形状:一段话 + 一张图,而且图的字段名必须是 `image_url`。
    #[test]
    fn 看图消息是文本加图片() {
        let message = Message::user_with_image("看这张图".to_string(), "data:image/png;base64,AAA".to_string());
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["role"], "user");
        let parts = value["content"].as_array().expect("带图时正文是数组");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,AAA");
        // 纯文本那条路**不能**被改成数组形状:它一直在跑,不是每个模型都吃数组
        let plain = serde_json::to_value(Message::user("你好".to_string())).unwrap();
        assert!(plain["content"].is_string(), "{plain}");
    }

    /// 出片尺寸只从服务商那七档里挑,而且要贴屏幕。
    #[test]
    fn 出片尺寸从七档里挑最贴屏幕的() {
        // 屏幕本身就在清单里 → 原样,一个像素都不用放大
        assert_eq!(video_size_for((1920, 1080)), (1920, 1080));
        assert_eq!(video_size_for((1080, 1920)), (1080, 1920));
        assert_eq!(video_size_for((3840, 2160)), (3840, 2160));
        // 16:10 的屏:清单里没有这一档 → 比例最近的 16:9 里,面积不超过屏幕的最大者
        assert_eq!(video_size_for((2560, 1600)), (1920, 1080));
        // 略小于 720p 的屏
        assert_eq!(video_size_for((1366, 768)), (1280, 720));
        // 32:9 的带鱼屏:2048×1080 的比例(1.896)比 16:9 更贴
        assert_eq!(video_size_for((5120, 1440)), (2048, 1080));
        // 4:3 的小屏:**两档的比例距离完全相等**,不能因此给横屏塞一档竖屏尺寸
        let (width, height) = video_size_for((800, 600));
        assert!(width > height, "横屏挑出来的是竖屏尺寸:{width}x{height}");
    }

    /// 挑出来的那一档**必须**在清单里,而且**每条边都是偶数** ——
    /// 这条不是形式主义:改动之前正是"服务端自己推、推出奇数长边"把片子做死的(见 `VIDEO_SIZES`)。
    #[test]
    fn 出片尺寸永远是清单里的偶数档() {
        for screen in [
            (1280, 800),
            (1920, 1080),
            (2560, 1440),
            (3440, 1440),
            (5120, 1440),
            (1024, 768),
            (768, 1024),
            (3840, 2160),
            (1920, 1200),
        ] {
            let size = video_size_for(screen);
            assert!(
                VIDEO_SIZES.contains(&size),
                "{screen:?} 挑出了清单外的 {size:?}"
            );
            assert_eq!(size.0 % 2 + size.1 % 2, 0, "{size:?} 有奇数边");
        }
    }

    /// 这个"计算器"必须和实测对得上 —— 整条修法都架在它上面。
    #[test]
    fn 服务商自己推出来的尺寸和实测一致() {
        // 两张出过片的 `1808×1136`:成片元数据里就是 1718×1080
        assert_eq!(derived_video_size((1808, 1136)), (1718, 1080));
        // 三次 FAIL 的那张:`1080 × 1920/1088 = 1905.88` → 向下取整 → 1905(奇数)
        assert_eq!(derived_video_size((1920, 1088)), (1905, 1080));
        // 竖屏反过来:短边是宽
        assert_eq!(derived_video_size((1088, 1920)), (1080, 1905));
        // 已经落在官方档上的一点都不用动
        assert_eq!(derived_video_size((1920, 1080)), (1920, 1080));
    }

    /// 判据只有一条:**推出来的长边是偶数**。奇数才裁,安全的一律原样发。
    #[test]
    fn 只有推出来是奇数才需要裁图() {
        assert_eq!(video_image_fix((1808, 1136)), None);
        assert_eq!(video_image_fix((1920, 1080)), None);
        // 1920×1088 → 1905(奇数)→ 裁到最贴它的那一档(16:9)
        assert_eq!(video_image_fix((1920, 1088)), Some((1920, 1080)));
        // 竖屏同理,且必须是竖屏的档
        assert_eq!(video_image_fix((1088, 1920)), Some((1080, 1920)));
        // 奇数高度 / 正方形也要能落到清单里的某一档
        for image in [(1000, 1000), (1001, 1000), (1000, 1001), (3000, 2000)] {
            if let Some(size) = video_image_fix(image) {
                assert!(VIDEO_SIZES.contains(&size), "{image:?} 挑出了 {size:?}");
            }
        }
    }

    /// **这条是整条修法的命根子**:裁成官方某一档的比例之后,服务商推出来的长边必须是偶数。
    /// 否则"裁了还是死",而它死得一声不吭 —— 只有真跑一次才知道。
    #[test]
    fn 官方七档自己推出来都是偶数() {
        for size in VIDEO_SIZES {
            let derived = derived_video_size(size);
            assert_eq!(
                derived.0 % 2 + derived.1 % 2,
                0,
                "{size:?} 自己会推出 {derived:?},有奇数边"
            );
        }
    }
}
