//! MC 壁纸 · 对话 → 「壁纸规格」的模型客户端(OpenAI 兼容接口)。
//!
//! 方案 3.1 定的路子是**不让模型画图**:模型只输出一份 `WallpaperSpec` JSON,本地着色器毫秒级
//! 渲染。这个 crate 就干这一件事 —— 一次调用、严格 JSON、失败了本地降级。
//!
//! 几个刻意的选择:
//! - **一次调用,不搞"提示词智能体"**:参数空间已经被 4 个预设和取值范围锁死,"设计提示词"在这个
//!   环节里等价于"填参数",再套一层只会把成本、延迟、失败点全翻倍。真需要两步(生成 → 自评 →
//!   改提示词)是位图那条路的事,而且也得等 A 档跑通、发现效果不够之后再上。
//! - **绝不返回 Err 给用户**:`draft()` 要么给出模型的结果,要么给出本地降级(方案 3.4 的 L1),
//!   调用方拿到的永远是一份能渲染的 spec。用户敲了字,桌面总得有反应,不能弹个错误框了事。
//! - **超时 45 s**:方案里那句"LLM 超时默认 3 s"是给 T1 环境联动用的;这里是用户敲完字等结果的
//!   T3/T2,3 s 太紧(中文长回复常态 5~15 s)。调用跑在后台线程 + 面板显示"生成中…",等得起。
//! - **不传 `response_format: json_object`**:各家的兼容实现对这个字段态度不一(有的直接 400),
//!   提示词里已经写死"只输出 JSON",再加一道本地提取,比赌服务商实现稳。
//! - **`thinking` 只在配置里显式写了才发**:智谱 GLM-4.7 系列默认开深度思考,一句"换张壁纸"也要先
//!   想十几秒;可这个字段别家不认(理由同 `response_format`)。所以做成可选 —— `ai.json` 里写了
//!   `"thinking": false` 才带上 `{"type":"disabled"}`,不写就完全不出现在请求体里。
//! - `image_prompt` 是**给以后留的钩子**:3.2 的 B 档(文生图 + 2.5D)要用,现在只落日志。

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use mcwp_spec::{Preset, Source, WallpaperSpec};
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

/// 请求超时(见文件头说明)。
const TIMEOUT: Duration = Duration::from_secs(45);

/// 模型回复里能塞给用户看的最大字符数,免得某家模型话痨把面板撑爆。
const MAX_REPLY_CHARS: usize = 120;

/// 走一次:模型给的这份规格 + 一句回复 + 它顺手写的画面描述。
pub struct Draft {
    /// 说给用户听的一句话
    pub reply: String,
    /// 可以直接渲染的规格(越界值已经在 `normalize()` 里夹紧)
    pub spec: WallpaperSpec,
    /// 画面描述,给以后接文生图用(现在只落日志)
    pub image_prompt: String,
    /// 用户的意图:只调参数,还是要一张新的
    pub mode: Mode,
    /// 是不是走了本地降级(模型没答上来)。面板拿它决定要不要跟一句"这次是内置预设顶的"。
    pub degraded: bool,
    /// 实际发出去的请求次数(降级路径可以是 1;没配 key 时是 0)
    pub calls: u32,
}

/// 用户的意图。方案 3.5 把"聊一句就换壁纸"拆成了 T2(指令)和 T3(显式创作):
/// T2 只改参数、不进壁纸库,T3 才是新壁纸 + 入库。这一区分交给模型判断最省事。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// "暗一点""别动那么快" —— 只改当前 spec 的参数
    Tweak,
    /// "换一张""我要赛博朋克" —— 新壁纸,该入壁纸库
    #[default]
    Create,
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

    /// 把一句话翻译成一份壁纸规格。**不会返回 Err**:失败就是本地降级。
    pub fn draft(&self, request: &str, current: &WallpaperSpec) -> Draft {
        let mut messages = vec![
            Message::system(system_prompt()),
            Message::user(first_prompt(request, current)),
        ];
        let mut calls = 0;
        let mut last_error = String::new();

        // 两次机会:第一次读不懂,把错误原样丢回去让它重写一遍 —— 比本地"猜着修 JSON"靠谱。
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
            match parse_reply(&text, request) {
                Ok(parsed) => {
                    tracing::info!(
                        "模型给的规格:{} | 预设={} | 指纹={} | 第 {} 次调用",
                        parsed.spec.title,
                        parsed.spec.preset().id(),
                        parsed.spec.spec_hash(),
                        attempt + 1
                    );
                    if !parsed.image_prompt.is_empty() {
                        tracing::info!("画面描述(留给以后的文生图):{}", parsed.image_prompt);
                    }
                    return Draft {
                        reply: parsed.reply,
                        spec: parsed.spec,
                        image_prompt: parsed.image_prompt,
                        mode: parsed.mode,
                        degraded: false,
                        calls,
                    };
                }
                Err(err) => {
                    last_error = format!("{err:#}");
                    tracing::warn!(
                        "模型这次给的 JSON 用不了(第 {} 次):{last_error}",
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

        tracing::warn!("模型这条路没走通({last_error}),改用本地降级");
        Draft {
            reply: format!(
                "模型没接上({}),先按内置预设给你换了一张。",
                friendly_reason(&last_error)
            ),
            spec: fallback_spec(request, current),
            image_prompt: String::new(),
            mode: Mode::Create,
            degraded: true,
            calls,
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
        parsed
            .choices
            .into_iter()
            .next()
            .map(|choice| choice.message.content)
            .filter(|content| !content.trim().is_empty())
            .ok_or_else(|| anyhow!("模型返回了空内容"))
    }
}

/// 解析模型那段文本,拆出 spec 与回复。
fn parse_reply(text: &str, request: &str) -> Result<Parsed> {
    let json = extract_json(text)?;
    let answer: ModelReply = serde_json::from_str(json).context("不是约定的 JSON 结构")?;
    // 模型有没有自己起名:`normalize()` 会把空标题补成预设名,但那对用户太没信息量了,
    // 所以先记下"它到底写没写",空的就用用户那句话来命名。
    let had_title = answer
        .spec
        .get("title")
        .and_then(|value| value.as_str())
        .is_some_and(|title| !title.trim().is_empty());
    let mut spec: WallpaperSpec =
        serde_json::from_value(answer.spec).context("spec 字段不是一个合法的壁纸规格")?;
    // 出处是"对话",库那边另说(`save_to_library` 会归一成快照)
    spec.source = Source::Conversation;
    // 越界的数值、缺的参数、多出来的键,全在这里被拉回合法范围
    spec.normalize();
    if !had_title {
        spec.title = title_from(request);
    }
    if spec.id.trim().is_empty() {
        spec.id = spec.spec_hash();
    }
    let reply = if answer.reply.trim().is_empty() {
        format!("已换成「{}」。", spec.title)
    } else {
        truncate(answer.reply.trim(), MAX_REPLY_CHARS)
    };
    Ok(Parsed {
        reply,
        spec,
        image_prompt: answer.image_prompt.unwrap_or_default().trim().to_string(),
        mode: answer.mode,
    })
}

struct Parsed {
    reply: String,
    spec: WallpaperSpec,
    image_prompt: String,
    mode: Mode,
}

/// 从模型那段文本里抠出 JSON 对象。
///
/// 提示词里写了"不要代码块",但免费模型经常还是裹一层 ```json —— 所以先拆围栏,再按
/// **第一个 `{` 到最后一个 `}`** 截:前后那句"好的,这是你要的规格:"也就一起被剥掉了。
fn extract_json(text: &str) -> Result<&str> {
    let start = text.find('{').ok_or_else(|| anyhow!("回复里没有 JSON"))?;
    let end = text.rfind('}').ok_or_else(|| anyhow!("JSON 没有闭合"))?;
    if end <= start {
        return Err(anyhow!("JSON 结构不完整"));
    }
    Ok(&text[start..=end])
}

/// 本地降级:关键词挑预设 + 情绪词调参数(方案 3.4 的 L1/L2)。
///
/// 目的是"永不黑屏":用户敲了字,桌面得有反应。识别不出来的就沿用当前预设并只调氛围。
fn fallback_spec(request: &str, current: &WallpaperSpec) -> WallpaperSpec {
    let matched = match_preset(request);
    let mut spec = match matched {
        Some(preset) if preset != current.preset() => WallpaperSpec::from_preset(preset),
        // 没有明确画面、或者就是当前这张:不换场景,只改氛围(L1)
        _ => current.clone(),
    };
    let lower = request.to_lowercase();
    if mentions(&lower, &["暗", "黑", "深", "夜", "dark"]) {
        spec.motion.intensity = (spec.motion.intensity * 0.7).clamp(0.0, 1.0);
    }
    if mentions(&lower, &["静", "稳", "慢", "温柔", "calm"]) {
        spec.motion.speed = (spec.motion.speed * 0.6).clamp(0.05, 1.0);
    }
    if mentions(&lower, &["动", "快", "活力", "energy", "赛博"]) {
        spec.motion.speed = 1.0;
        spec.motion.intensity = 1.0;
    }
    spec.title = title_from(request);
    spec.keywords = keywords_from(request);
    spec.source = Source::Conversation;
    spec.normalize();
    spec.id = spec.spec_hash();
    spec
}

/// 关键词 → 预设。识别不出来返回 `None`(调用方沿用当前预设)。
fn match_preset(request: &str) -> Option<Preset> {
    let lower = request.to_lowercase();
    let table: [(&[&str], Preset); 4] = [
        (
            &["雨", "夜", "城市", "窗", "rain", "city", "night"],
            Preset::RainyWindowCity,
        ),
        (
            &[
                "星", "宇宙", "银河", "极光", "云", "space", "nebula", "galaxy",
            ],
            Preset::NebulaDrift,
        ),
        (
            // 注意别写单字"纸":本行产品里"壁纸"两个字出现率最高,一个"纸"就把所有请求都
            // 判成极简纸感(假模型自检时当场踩到),所以只认"纸感""纸张"这种成词的。
            &[
                "极简", "留白", "素色", "纸感", "纸张", "几何", "minimal", "paper",
            ],
            Preset::MinimalPaper,
        ),
        (
            &[
                "专注",
                "网格",
                "写代码",
                "工作",
                "学习",
                "focus",
                "grid",
                "code",
            ],
            Preset::FocusGrid,
        ),
    ];
    table
        .into_iter()
        .find(|(words, _)| mentions(&lower, words))
        .map(|(_, preset)| preset)
}

fn mentions(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

/// 用用户那句话给壁纸起名:多空白折成一个空格,掐头 12 个字。
fn title_from(request: &str) -> String {
    let flat: String = request.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 12 {
        format!("{}…", flat.chars().take(12).collect::<String>())
    } else if flat.is_empty() {
        "新的壁纸".to_string()
    } else {
        flat
    }
}

/// 从一句话里抓几个关键词(降级路径用:这时没有模型给的 keywords)。
fn keywords_from(request: &str) -> Vec<String> {
    const MAX: usize = 5;
    request
        .split(|c: char| c.is_whitespace() || "，,。.、!！?？:：;；/|()（）「」".contains(c))
        .filter(|word| !word.is_empty())
        .take(MAX)
        .map(|word| word.chars().take(12).collect::<String>())
        .collect()
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

/// 提示词:把"能填什么"讲死,把"怎么答"讲死,其余交给模型。
fn system_prompt() -> String {
    format!(
        "你是「MC 壁纸」的场景设计师。用户会用一句话描述想要的桌面氛围,你把它翻译成一份\
         可以直接渲染的壁纸规格。\n\
         \n\
         **只输出一个 JSON 对象**,不要解释、不要 markdown 代码块、不要多余字段,结构:\n\
         {{\n\
         \x20 \"reply\": \"对用户说的一句中文回复(30 字以内,说清楚你做了什么)\",\n\
         \x20 \"mode\": \"tweak\" 或 \"create\",\n\
         \x20 \"image_prompt\": \"同一画面的中文描述(一句话,给以后的文生图用)\",\n\
         \x20 \"spec\": {{ 壁纸规格,字段见下 }}\n\
         }}\n\
         \n\
         spec 的字段:\n\
         - title:中文短标题(6~12 字,别用书名号)\n\
         - keywords:2~5 个中文关键词(如 雨 / 夜 / 冷蓝)\n\
         - mood:calm / focus / energy 三个 0~1 的数\n\
         - palette:恰好 5 个 #RRGGBB 颜色,从深到浅(第一个是底色;桌面壁纸常年在图标下面,\
         别用大面积纯白)\n\
         - scene.preset:只能取下面目录里的 id\n\
         - scene.params:只能填该预设声明的参数(key 必须一字不差),值必须落在给定范围内\n\
         - motion.intensity / motion.speed:0~1\n\
         - lighting.keyLight:0~1;lighting.followDesktopBrightness 建议 true\n\
         - seed:任意整数。**它决定随机细节,保持不变就能复现同一张壁纸**\n\
         \n\
         mode 怎么定:\n\
         - 用户在说\"暗一点 / 冷一点 / 别动那么快\"这类**改动**时,填 \"tweak\",其余字段尽量与\
         【当前壁纸】保持一致,只动该动的那几个;\n\
         - 用户在说\"换一张 / 生成一张 / 我要赛博朋克\"这类**新作品**时,填 \"create\"。\n\
         \n\
         <预设目录>\n\
         {}\n\
         </预设目录>\n\
         \n\
         规则:拿不准就给中间值,不要给极端值;字段缺失按默认值处理,但**不要发明新字段**。",
        mcwp_spec::presets::catalog()
    )
}

/// 第一轮的用户消息:要什么 + 现在是什么。
fn first_prompt(request: &str, current: &WallpaperSpec) -> String {
    format!(
        "【当前壁纸】\n{}\n\n【用户说】\n{}",
        current.canonical_json(),
        request
    )
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

/// 模型该给出的那个 JSON 对象。
#[derive(Debug, Deserialize)]
struct ModelReply {
    #[serde(default)]
    reply: String,
    #[serde(default)]
    mode: Mode,
    #[serde(default)]
    image_prompt: Option<String>,
    /// 先当 `Value` 收着再单独解析:这样"外层对、spec 不对"能报出更准的错,重试时也说得清
    spec: serde_json::Value,
}
