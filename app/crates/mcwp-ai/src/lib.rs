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
use mcwp_spec::{MotionKind, MotionRecipe, Photo};
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

/// 默认生图服务商。还是智谱、还是同一把 key —— 它的免费档 `cogview-3-flash` 实测能出图
/// (9~16 s 一张,100~230 KB),所以我们不额外引入第二家。
///
/// 免费档的代价是**右下角有"AI生成"水印**(约占宽 9% × 高 5%)。这不影响使用:
/// 壁纸本来就按 cover 裁,把 UV 往左上挪一点点就把它推出画面了(见 `mcwp-render/src/photo.wgsl`)。
/// 要彻底没水印就换一家(硅基流动的 Kolors / FLUX.1-schnell 也免费),所以这里必须是配置项。
pub const DEFAULT_IMAGE_BASE_URL: &str = DEFAULT_BASE_URL;

/// 默认生图模型。`cogview-3-flash` 是智谱的免费档 —— 想换 `cogview-3` / `cogview-4` 直接改配置。
pub const DEFAULT_IMAGE_MODEL: &str = "cogview-3-flash";

/// 默认的「图生视频」模型。
///
/// `cogvideox-3` 实测(2026-10-16):提交 200、**约 61 s 出片**、1080×1080、5.13 s、31 fps、带水印,
/// mp4 828 KB(H.264)。另有一个 `cogvideox-flash` 的名字,但**当前一直回 429**
/// (错误码 1305「该模型当前访问量过大」),所以默认不选它 —— 想试就改配置。
///
/// 它产出的 mp4 **地址只有 24 小时有效**(生图那批是 7 天),所以和生图一样:
/// 拿到就当场下载,绝不只存地址。
pub const DEFAULT_VIDEO_MODEL: &str = "cogvideox-3";

/// 轮询那条异步接口的间隔。它一次要跑几十秒,查得太密没有意义,太疏又会让"好了"晚到几秒。
const VIDEO_POLL_INTERVAL: Duration = Duration::from_secs(8);

/// 一条视频最多等多久。实测 61 s,给到 6 分钟 —— 到点就报错,
/// 而不是让面板上那颗「做成动态壁纸」永远转下去。
const VIDEO_WAIT_TIMEOUT: Duration = Duration::from_secs(360);

/// 动效提示词最长多少字。图生视频那句**只描述"怎么动"**,不需要长;太长反而会被当成分镜。
const MAX_MOTION_PROMPT_CHARS: usize = 200;

/// 生图接口对尺寸的硬要求(智谱 CogView 实测):宽高都得是 **16 的整数倍**、各自落在
/// 512~2880,且总像素不超过 2²¹。
///
/// 最容易踩的是第一条 —— `1920×1080` 会被**直接拒绝**,因为 1080/16 = 67.5。
/// 1080p 屏得请求 `1920×1088` 再由渲染器按 cover 裁掉那 8 个像素。
const SIZE_MIN: u32 = 512;
const SIZE_MAX: u32 = 2880;
const SIZE_PIXEL_MAX: u64 = 1 << 21;

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
    /// 「图生视频」那一套。它和生图**基本总是同一家**(同一把 key 的同一个接口域下),
    /// 所以这里只留一个模型名:地址和 key 都从生图 / 对话那两组继承下来(见 `VideosConfig::load`)。
    /// 真要用另一家,再补字段不迟 —— 现在多两个输入框只会让人不知道该填哪个。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub video_model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub video_base_url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub video_api_key: String,
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
         \"api_key\": \"你的key\", \"image_model\": \"{DEFAULT_IMAGE_MODEL}\"}}\n\
         生图默认**跟对话共用同一把 key**(智谱一把 key 管两个接口),想分开就再填 \
         image_base_url / image_model / image_api_key。\n\
         临时试一家也可以只设环境变量 MCWP_IMAGE_KEY / MCWP_IMAGE_BASE_URL / MCWP_IMAGE_MODEL。"
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
}

impl Images {
    pub fn new(cfg: ImagesConfig) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            // 生图的等待时间比对话长得多(实测 9~16 s,高峰期更久),所以给到 3 分钟
            .timeout_global(Some(Duration::from_secs(180)))
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
        format!("{host} · {}", self.cfg.model)
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
    pub fn generate(&self, prompt: &str, width: u32, height: u32) -> Result<GeneratedImage> {
        let (width, height) = fit_size(width, height);
        let body = serde_json::to_string(&ImageRequest {
            model: self.cfg.model.as_str(),
            prompt,
            size: format!("{width}x{height}"),
        })?;
        let mut response = self
            .agent
            .post(&self.endpoint())
            .header("Content-Type", "application/json")
            .header("Authorization", &format!("Bearer {}", self.cfg.api_key))
            .send(body.as_str())
            .map_err(|err| anyhow!("请求生图失败:{err}"))?;
        let status = response.status();
        let raw = response.body_mut().read_to_string().context("读生图响应失败")?;
        if !status.is_success() {
            return Err(anyhow!(
                "生图接口返回 {}:{}",
                status.as_u16(),
                truncate(&raw, 300)
            ));
        }
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
            "生图完成:{}×{} · {} KB · {}",
            width,
            height,
            bytes.len() / 1024,
            truncate(prompt, 60)
        );
        Ok(GeneratedImage { url, bytes })
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
    pub fn submit(&self, image_url: &str, prompt: &str) -> Result<String> {
        let body = serde_json::to_string(&VideoRequest {
            model: self.cfg.model.as_str(),
            prompt,
            image_url,
        })?;
        let mut response = self
            .agent
            .post(&format!("{}/videos/generations", self.root()))
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
        tracing::info!("图生视频任务已提交:{}", task.id);
        Ok(task.id)
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
            "FAIL" => Ok(VideoProgress::Failed(truncate(&raw, 200))),
            // PROCESSING 是实测见到的;别的状态一律当"还在跑"—— 宁可多等一轮,
            // 也不要把一个未知状态当成失败把任务扔掉(它可能已经把额度花了)。
            other => {
                tracing::debug!("图生视频还在跑:{other}");
                Ok(VideoProgress::Running)
            }
        }
    }

    /// 整条路:提交 → 轮询到出片 → **当场下载**。
    pub fn animate(&self, image_url: &str, prompt: &str) -> Result<GeneratedVideo> {
        let id = self.submit(image_url, prompt)?;
        let started = std::time::Instant::now();
        loop {
            match self.poll(&id)? {
                VideoProgress::Running => {
                    if started.elapsed() > VIDEO_WAIT_TIMEOUT {
                        return Err(anyhow!(
                            "等了 {:.0} 分钟还没出片(任务 {id} 可能还在排队),先别管它,过会儿再试",
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
                    return Err(anyhow!("服务商说这条视频没生成出来:{reason}"));
                }
            }
        }
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
        let bytes = response.body_mut().read_to_vec().context("读视频字节失败")?;
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

/// 把"屏幕分辨率"折成服务商能接受的尺寸(规则见 `SIZE_MIN` 那段)。
///
/// 步骤:等比缩到总像素上限内 → 夹到 512~2880 → 四舍五入到 16 的倍数 → 再保一次总像素。
/// 最后那一步不是多余:`2880×2880` = 829 万像素 > 2²¹,贴到上界时确实会超。
pub fn fit_size(width: u32, height: u32) -> (u32, u32) {
    let mut width = width.max(1).min(SIZE_MAX);
    let mut height = height.max(1).min(SIZE_MAX);
    let pixels = width as u64 * height as u64;
    if pixels > SIZE_PIXEL_MAX {
        let scale = (SIZE_PIXEL_MAX as f64 / pixels as f64).sqrt();
        width = ((width as f64 * scale) as u32).max(1);
        height = ((height as f64 * scale) as u32).max(1);
    }
    // 四舍五入而不是向下取整:1080 → 1088(而不是 1072),少裁一点是一点
    let snap = |value: u32| -> u32 { ((value + 8) / 16 * 16).clamp(SIZE_MIN, SIZE_MAX) };
    width = snap(width);
    height = snap(height);
    while width as u64 * height as u64 > SIZE_PIXEL_MAX && width > SIZE_MIN {
        width = snap(width - 16);
        height = snap(height - 16);
    }
    (width, height)
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

    /// 发一次 chat/completions,取回模型那段文本。
    fn chat(&self, messages: &[Message]) -> Result<String> {
        let body = serde_json::to_string(&ChatRequest {
            model: self.cfg.model.as_str(),
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
你在为「图生视频」写提示词。用户会给你一张**已经画好的图**的描述,你要写的不是画面里有什么\
(那个模型已经看得见了),而是**它该怎么动**。\n\
只写一句话,不超过 60 字,内容只允许落在下面三类里:\n\
1. 镜头:缓慢推近 / 缓慢拉远 / 左右平移 / 轻微环绕 / 固定机位\n\
2. 画面内的自然运动:云在飘、水面泛起波纹、雾气缓缓流动、树叶轻晃、光影缓慢变化\n\
3. 节奏:整体缓慢、平稳\n\
**绝对不要出现**:画面里有什么东西、文字、字母、数字、水印、logo、人脸特写、人物主体、\
分镜切换、剧烈缩放、闪烁。\n\
只输出这一句话本身:不要解释、不要加引号、不要 JSON、不要 markdown、不要换行。";

/// 交给模型的"这张图是什么"。
fn motion_request(photo: &Photo) -> String {
    format!(
        "这张壁纸叫「{}」,画面描述是:{}\n请写一句它该怎么动的提示词。",
        photo.title, photo.prompt
    )
}

/// 模型没答上来时用的通用运镜。**必须是一句真能用的话**:这条路是兜底,
/// 用户点完按钮总得有东西动起来,而不是弹一句"模型没接上"。
/// 公开出去是因为调用方也可能直接用它:没配对话模型时,图生视频那条路照样要能跑。
pub fn fallback_motion_prompt() -> &'static str {
    "镜头缓慢推近,画面里的云与雾缓缓流动、水面有细微波纹、光影轻微变化,整体平稳自然的慢镜头"
}

/// 把模型那句回复收干净:去围栏、去引号、只留第一行、限长。
///
/// 免费模型很爱加一层 ``` 或者"提示词:"这样的前缀;而 `image_url` 那一侧只吃一句话,
/// 多出来的字会变成视频里莫名其妙的东西,所以宁可在本地削干净。
fn clean_motion_reply(text: &str) -> String {
    // 逐行看:跳过围栏那一行、摘掉"提示词:"这类前缀,取**第一行还有内容**的当正文。
    // 不能只 `find` "不以提示词开头的行" —— 正文和前缀写在同一行是很常见的返法。
    const PREFIXES: [&str; 6] = ["提示词:", "提示词：", "输出:", "输出：", "Prompt:", "prompt:"];
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
        if !line.is_empty() {
            picked = line.to_string();
            break;
        }
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
    content: String,
}

impl Message {
    fn system(content: String) -> Self {
        Self {
            role: "system",
            content,
        }
    }

    fn user(content: String) -> Self {
        Self {
            role: "user",
            content,
        }
    }

    fn assistant(content: String) -> Self {
        Self {
            role: "assistant",
            content,
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

/// 图生视频的请求体。**只有三个字段**,而且每个都是必需的:
/// `image_url` 是那张图(base64 内联,见 `image_data_url`),`prompt` 是"它该怎么动"。
/// 服务商那边还有尺寸 / 时长 / 音轨这些可选参数,但它们都由模型自己定 —— 壁纸不需要挑,
/// 多写两个字段只会多两处会过期的假设。
#[derive(Debug, Serialize)]
struct VideoRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    image_url: &'a str,
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

    /// 尺寸这条规则是**实测出来的**,不是猜的:服务商要求宽高都是 16 的倍数、各自 512~2880、
    /// 总像素 ≤ 2²¹。`1920×1080` 会被直接拒绝(1080/16 = 67.5),这里钉住"折成合法的"。
    #[test]
    fn 生图尺寸总是十六的倍数且落在区间里() {
        assert_eq!(fit_size(1920, 1080), (1920, 1088));
        assert_eq!(fit_size(1600, 900), (1600, 896));
        assert_eq!(fit_size(1024, 1024), (1024, 1024));

        // 4K:两条上界(边长 2880、总像素 2²¹)都要被照顾到
        for (width, height) in [(3840u32, 2160u32), (2560, 1440), (1280, 720), (800, 600)] {
            let (width, height) = fit_size(width, height);
            assert_eq!(width % 16, 0, "{width} 不是 16 的倍数");
            assert_eq!(height % 16, 0, "{height} 不是 16 的倍数");
            assert!((SIZE_MIN..=SIZE_MAX).contains(&width), "宽 {width} 越界");
            assert!((SIZE_MIN..=SIZE_MAX).contains(&height), "高 {height} 越界");
            assert!(
                width as u64 * height as u64 <= SIZE_PIXEL_MAX,
                "{width}×{height} 总像素超了"
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
        let long = clean_motion_reply(&"推".repeat(500));
        assert!(long.ends_with('…'));
        assert!(long.chars().count() <= MAX_MOTION_PROMPT_CHARS + 1);
    }
}
