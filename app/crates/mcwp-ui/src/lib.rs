//! MC 壁纸 · 控制面板(Slint 实现)。
//!
//! 线程模型:面板跑在自己的线程 + 自己的 Slint 事件循环里,主线程专心伺候壁纸的
//! DXGI 呈现循环。两个方向的数据流:
//!
//! ```text
//!   UI 线程  --UiEvent 通道-->  壁纸主循环    (选预设 / 拖参数 / 存为壁纸)
//!   UI 线程  <--PanelHandle--   壁纸主循环    (状态文字、回填参数)
//! ```
//!
//! 面板本身不存任何东西:真相永远是壁纸侧的 `WallpaperSpec`,`UiEvent` 只是"用户动了什么"。
//! 关闭窗口 = 事件循环退出 = 线程结束,面板占的内存整体还给系统(方案里的"按需建窗")。
//!
//! 视觉是 iOS 26 的液态玻璃风格,细节见 `ui/panel.slint` 顶部的说明;玻璃色由 `theme`
//! 从当前壁纸的调色板推出来,窗口是无边框的,DWM 圆角与拖动在 `mcwp_platform::glass_window`。

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::{anyhow, Result};
use mcwp_platform::glass_window::{WindowDrag, WindowZoom, apply_glass, minimize};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::ComponentHandle;
use slint::winit_030::WinitWindowAccessor;

mod theme;

pub use theme::glass_colors;

slint::include_modules!();

/// 内嵌子集字体覆盖到的字符表,由 `tools/subset-panel-font.py` 生成(跟着字体一起走)。
const PANEL_CHARSET: &str = include_str!("../fonts/panel-charset.txt");

/// 托盘图标(`tools/make-icon.py` 画的那份 .ico)的原始字节。
///
/// 托盘走 Win32 原生的 `Shell_NotifyIconW`,要的是 .ico 文件字节(自己解析目录取一张图,
/// 不用它落盘)。图标本来就是这个 crate 的资产,所以由这里 `include_bytes!` 编进 exe。
pub const TRAY_ICON_ICO: &[u8] = include_bytes!("../assets/mcwp-icon.ico");

/// 找出 `text` 里面板字体会渲染成空白的字符(去重,按首次出现的顺序)。
///
/// 面板用的是子集字体(只含面板文案用到的字符,见 `tools/subset-panel-font.py`),
/// 缺字不会报错、只会画不出来。动态文案(预设标题,以后 AI 生成的壁纸名)一旦冒出
/// 表外字符,就把返回值记进日志,好及时重跑子集化脚本。
pub fn missing_chars(text: &str) -> Vec<char> {
    let covered: HashSet<char> = PANEL_CHARSET.chars().collect();
    let mut seen = HashSet::new();
    text.chars()
        .filter(|ch| !ch.is_whitespace() && !covered.contains(ch))
        .filter(|ch| seen.insert(*ch))
        .collect()
}

/// 面板 UI 需要的数据类型,转出去给壁纸进程拼 `PanelState` 用。
pub use slint::{ModelRc, SharedString, VecModel};

/// 面板发回壁纸进程的事件。
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// 用户在输入框里发了一条消息(生图界面)
    SendMessage(String),
    /// 点开了左栏里的某个会话(左栏下标)
    SelectConversation(usize),
    /// 点了「+ 新建对话」
    NewConversation,
    /// 左栏右键菜单:把第 `index` 个会话改名(两端空白由壁纸侧清,清完是空名字就不改)
    RenameConversation(usize, String),
    /// 左栏右键菜单:删掉第 `index` 个会话
    DeleteConversation(usize),
    /// 左栏右键菜单:置顶 / 取消置顶第 `index` 个会话
    TogglePinConversation(usize),
    /// 左栏右键菜单:把第 `index` 个会话标为未读(不切过去)
    MarkConversationUnread(usize),
    /// 面板切到「壁纸库」页:想要一份最新的库列表
    RefreshLibrary,
    /// 设置页点了「保存」:接口地址 / 模型名 / API Key / 深度思考。
    /// 校验、落盘、重建客户端全在壁纸侧 —— 面板不碰文件系统(和会话、壁纸库一个规矩)。
    SaveAiConfig {
        base_url: String,
        model: String,
        api_key: String,
        thinking: ThinkingChoice,
        /// 生图那一组;留空 = 跟着上面那三个走
        image_base_url: String,
        image_model: String,
        image_api_key: String,
        /// 图生视频的模型名;留空 = 用内置默认
        video_model: String,
    },
    /// 对话进行中点「暂停」:这一轮的请求作废(结果回来会被丢掉)。
    PauseRun,
    /// 点「继续」:把暂停那一轮原样再跑一遍。
    ResumeRun,
    /// 点了壁纸库里的第 `index` 张(`PanelState::library` 的下标)
    OpenWallpaper(usize),
    /// 库那一格的右键菜单:删掉第 `index` 张
    DeleteWallpaper(usize),
    /// 把第 `index` 张静图「做成动态壁纸」(图生视频)。
    ///
    /// 和别的库操作一样,面板只递下标:那张是不是静图、够不够额度、结果往哪儿放,
    /// 全是壁纸侧的事。
    MakeVideo(usize),
    /// 同一件事,对象是**当前挂着的那张**(库页工具栏那颗钮)。
    MakeVideoFromCurrent,
    /// 「用当前壁纸」:把桌面上原来那张抬进来当作库里的新一张
    AdoptWallpaper,
    /// 「导入图片」:开系统选文件框,把选中的那张收进库里。选文件与入库都在壁纸侧做 ——
    /// 面板连"文件在哪"都不该知道(它不碰文件系统,这是它和其它几件事的共同规矩)。
    ImportWallpaper,
    /// 点了消息里那张预览图:把库里这张的**大图**打开(壁纸侧解码 → `PanelHandle::show_viewer`)
    OpenPhoto(String),
    /// 预览图的右键菜单「设为壁纸」:把这张换到桌面上去(生图不再自动换,这是"用这张"的入口)
    SetWallpaper(String),
    /// 大图那一层关掉了:把那份像素放掉
    ViewerClosed,
    /// 面板窗口被关掉了
    Closed,
}

/// 打开面板时喂给它的初始状态。
pub struct PanelState {
    pub title: String,
    pub hint: String,
    pub fingerprint: String,
    pub summary: String,
    /// 当前壁纸的调色板(`Photo::palette`),玻璃色由它推出来
    pub palette: Vec<String>,
    /// 生图界面:左栏历史 + 右栏会话
    pub session: SessionState,
    /// 壁纸库:由壁纸侧扫描库目录得到,面板只负责画(和会话数据同一个道理)
    pub library: Vec<LibraryEntryView>,
    /// 当前生效的壁纸在库里的 id;空串表示桌面上那张不在库里(比如刚抬进来的系统壁纸)
    pub library_current: String,
    /// 当前生效的那张在 `library` 里的下标;-1 = 不在库里。库页那颗「删除」靠它。
    pub library_current_index: i32,
}

/// 面板要显示的一条库记录(**能跨线程**的那份)。
///
/// 和 Slint 那个 `LibraryItem` 的唯一区别是缩略图:`slint::Image` **不是 `Send`**,
/// 带着它就过不了线程边界(`PanelState` 要移进面板线程)。所以这里放的是**裸像素** ——
/// RGBA8 + 宽高;贴进面板时(`apply_library`,那已经在面板线程里)才装成 `slint::Image`。
///
/// 为什么不是"把编码好的 PNG 送过去让 Slint 解":那要多开一个图片解码特性,
/// 而壁纸侧本来就有解码器(`mcwp-image`),解完送过来正好省掉面板这一步。
#[derive(Debug, Clone)]
pub struct LibraryEntryView {
    /// 照片 id(= 库目录里的文件夹名),面板靠它和「当前是哪张」比对
    pub id: String,
    pub title: String,
    /// 卡片上那行小字(提示词掐短)
    pub hint: String,
    pub summary: String,
    pub accent: slint::Color,
    /// 这一条是不是「动态壁纸」(视频)。面板靠它在格子角上画一个「动」标 ——
    /// 库里两种东西混着放,没有这个标就得点开才知道。
    pub video: bool,
    /// (宽, 高, RGBA8 像素)。**必须和宽高对得上**,不然贴出来是花的。
    pub thumb: (u32, u32, Vec<u8>),
}

/// 设置页要显示的模型配置。
///
/// 它**故意不进 `PanelState`**:整块 `refresh()` 会在换壁纸、切会话时随时触发,而设置页那几个
/// 输入框里可能正是用户敲到一半的 key —— 那样会被整块回填冲掉。所以这份单独走
/// `PanelHandle::refresh_ai()`,只在「打开面板」和「保存成功」两个时刻灌一次。
#[derive(Debug, Clone, Default)]
pub struct AiConfigState {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    /// 生图那一组。**留空是合法的**:空 = 跟着上面那三个走(见 `mcwp_ai::ImagesConfig::load`),
    /// 所以"同一家服务商"时用户只需要填上面那组。
    pub image_base_url: String,
    pub image_model: String,
    pub image_api_key: String,
    /// 图生视频的模型名。**留空是合法的**:空 = 用内置默认(`cogvideox-3`)。
    /// 地址和 Key 不在这儿 —— 图生视频和生图基本总是同一家,那两个跟着生图那组走。
    pub video_model: String,
    /// 「深度思考」选的哪一档
    pub thinking: ThinkingChoice,
    /// 有 `MCWP_AI_*` 环境变量在生效:界面上要提醒一句,不然用户改完发现没反应。
    pub env_override: bool,
}

/// 设置页里「深度思考」这一项。
///
/// 三态是必须的:落盘时 `None`(**不发这个字段**)和 `Some(false)`(发 `disabled`)是两个不同的
/// 请求体 —— 不认 `thinking` 的服务商只能走前者(传了可能直接 400),而智谱 4.7 系列又想显式关掉它。
/// 一个布尔开关表达不了这三件事,所以界面上是一排小胶囊。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThinkingChoice {
    /// 请求体里不带这个字段,由服务商自己定(不认 `thinking` 的服务商选这个)
    #[default]
    Unset,
    /// 显式打开
    Enabled,
    /// 显式关掉
    Disabled,
}

impl ThinkingChoice {
    /// 面板传上来的是一排胶囊的下标(0/1/2);认不出的按「不发」处理,别让手滑改坏请求体。
    fn from_index(index: i32) -> Self {
        match index {
            1 => Self::Enabled,
            2 => Self::Disabled,
            _ => Self::Unset,
        }
    }

    fn index(self) -> i32 {
        match self {
            Self::Unset => 0,
            Self::Enabled => 1,
            Self::Disabled => 2,
        }
    }

    /// 落进 `ai.json` 的样子。`None` 意味着那个字段根本不写出来。
    pub fn to_option(self) -> Option<bool> {
        match self {
            Self::Unset => None,
            Self::Enabled => Some(true),
            Self::Disabled => Some(false),
        }
    }

    /// 从 `ai.json` 读回来的反向映射。老配置里没有这个字段就是 `None` → 「不发」。
    pub fn from_option(value: Option<bool>) -> Self {
        match value {
            None => Self::Unset,
            Some(true) => Self::Enabled,
            Some(false) => Self::Disabled,
        }
    }
}

/// 这一轮对话跑到哪儿了 —— 面板靠它决定右下角那颗钮长什么样。
///
/// 状态归壁纸侧(Rust)所有:请求是在那边发出去的,只有它知道"结果还没回来"。面板只是投影,
/// 所以走 `PanelHandle::set_run_state()` 单独灌,不进 `SessionState`(理由同 `AiConfigState`:
/// 会话整块回填会因为换壁纸、切会话随时发生,而这里最需要的是"准,但不跟着乱刷")。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    /// 闲着:可以发新消息
    Idle,
    /// 正在跑:发送键变成暂停圆钮
    Running,
    /// 已暂停:输入框空着时是「继续」,一旦打字就变回「发送」
    Paused,
}

impl RunState {
    /// 面板那边收的是整数:0 / 1 / 2。
    fn index(self) -> i32 {
        match self {
            Self::Idle => 0,
            Self::Running => 1,
            Self::Paused => 2,
        }
    }
}

/// 生图界面的会话数据:左栏的会话列表 + 当前会话的消息。
///
/// 面板不持有任何状态,所以换会话、发消息都是「Rust 改数据 → 整块回填」。`revision`
/// 每改一次 +1:消息列表靠它把滚动位置钉到底部(内容长高了才需要滚,面板自己看不出来)。
///
/// 数据本身由壁纸侧落盘(和壁纸库同一个目录),面板这一侧永远只是投影 —— 它不碰文件系统。
#[derive(Debug, Clone, Default)]
pub struct SessionState {
    /// 左栏:历史对话
    pub conversations: Vec<ConversationItem>,
    /// 当前选中的会话(左栏下标)
    pub current: usize,
    /// 右栏标题
    pub title: String,
    /// 当前会话的消息(按时间顺序)
    pub messages: Vec<ChatMessage>,
    /// 消息版本号
    pub revision: i32,
}

impl SessionState {
    /// 会话列表与消息里,内嵌字体画不出来的字符。用户输入的字面覆盖不到是常态,
    /// 所以调用方拿到非空结果时按「记一笔」处理就好,不用当错误。
    pub fn missing_chars(&self) -> Vec<(String, Vec<char>)> {
        let mut fields: Vec<(String, String)> = Vec::new();
        fields.push(("标题".into(), self.title.clone()));
        for (index, conversation) in self.conversations.iter().enumerate() {
            fields.push((format!("会话{index} 标题"), conversation.heading.to_string()));
            fields.push((format!("会话{index} 预览"), conversation.sub.to_string()));
        }
        for (index, message) in self.messages.iter().enumerate() {
            fields.push((format!("消息{index}"), message.text.to_string()));
        }
        fields
            .into_iter()
            .filter_map(|(name, text)| {
                let missing = missing_chars(&text);
                (!missing.is_empty()).then_some((name, missing))
            })
            .collect()
    }
}

impl PanelState {
    /// 面板上会显示、但内嵌字体画不出来的字符,按「来源 → 缺字」列出来。
    pub fn missing_chars(&self) -> Vec<(String, Vec<char>)> {
        let mut fields: Vec<(String, String)> = vec![
            ("标题".into(), self.title.clone()),
            ("关键词".into(), self.hint.clone()),
            ("说明".into(), self.summary.clone()),
        ];
        // 库里的壁纸名是动态的(AI 起的标题),同样要盯缺字
        for (index, item) in self.library.iter().enumerate() {
            fields.push((format!("库{index} 标题"), item.title.to_string()));
            fields.push((format!("库{index} 说明"), item.summary.to_string()));
        }
        let mut missing_fields: Vec<(String, Vec<char>)> = fields
            .into_iter()
            .filter_map(|(name, text)| {
                let missing = missing_chars(&text);
                (!missing.is_empty()).then_some((name, missing))
            })
            .collect();
        // 会话标题与消息是用户/模型给的动态字面,缺字是常态,和上面那些静态文案一样列出来
        missing_fields.extend(self.session.missing_chars());
        missing_fields
    }
}

/// 面板句柄,可以安全地跨线程持有。
pub struct PanelHandle {
    weak: slint::Weak<Panel>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PanelHandle {
    /// 设底部状态文字(「已存为壁纸 · a1b2」之类)。
    pub fn set_status(&self, text: impl Into<String>) {
        let weak = self.weak.clone();
        let text: String = text.into();
        let missing = missing_chars(&text);
        if !missing.is_empty() {
            tracing::warn!(
                "面板状态文字有 {} 个字符不在内嵌字体内:{:?}",
                missing.len(),
                missing
            );
        }
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(panel) = weak.upgrade() {
                panel.set_status(text.into());
            }
        });
    }

    /// 整块回填会话视图(左栏列表 + 当前会话的消息)。发消息 / 切会话 / 新建会话都走它。
    pub fn refresh_session(&self, session: SessionState) {
        let weak = self.weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(panel) = weak.upgrade() {
                apply_session(&panel, &session);
            }
        });
    }

    /// 打开「看大图」那一层。像素由壁纸侧现解现送(缩略图放大看是糊的)。
    pub fn show_viewer(&self, title: String, pixels: (u32, u32, Vec<u8>)) {
        let weak = self.weak.clone();
        let (width, height, rgba) = pixels;
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(panel) = weak.upgrade() {
                let buffer = slint::SharedPixelBuffer::clone_from_slice(&rgba, width, height);
                panel.set_viewer(slint::Image::from_rgba8(buffer));
                panel.set_viewer_title(title.into());
                panel.set_viewing(true);
            }
        });
    }

    /// 关掉大图:**把那份像素换掉**。
    ///
    /// 一行 `set_viewing(false)` 只是不画它了,几 MB 到几十 MB 的 RGBA 会一直挂在窗口上
    /// (面板是个常驻窗口,不是一次性进程)。所以这里连图一起换成空的。
    pub fn clear_viewer(&self) {
        let weak = self.weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(panel) = weak.upgrade() {
                panel.set_viewing(false);
                panel.set_viewer(slint::Image::default());
                panel.set_viewer_title(Default::default());
            }
        });
    }

    /// 回填设置页的模型配置。
    ///
    /// **只在「面板刚打开」和「保存成功」两个时刻调**:设置页那几个输入框平时是用户在编辑,
    /// 而 `refresh()` 会因为换壁纸 / 切会话随时整块回填 —— 把配置塞进 `PanelState` 会把
    /// 用户正在敲的字冲掉。理由见 `AiConfigState`。
    pub fn refresh_ai(&self, ai: AiConfigState) {
        let weak = self.weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(panel) = weak.upgrade() {
                apply_ai(&panel, &ai);
            }
        });
    }

    /// 告诉面板"这一轮对话跑到哪儿了":换了就灌一次。
    ///
    /// 只在**起跑 / 暂停 / 落地**这三个时刻调 —— 那颗钮的样子完全由它决定,漏一次就会停在
    /// 错的形态上(比如跑完了还显示暂停键)。
    pub fn set_run_state(&self, state: RunState) {
        let weak = self.weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(panel) = weak.upgrade() {
                panel.set_run_state(state.index());
            }
        });
    }

    /// 请求关闭面板(等价于用户点右上角)。
    pub fn request_close(&self) {
        let _ = slint::invoke_from_event_loop(|| {
            let _ = slint::quit_event_loop();
        });
    }

    /// 用一份新的面板状态整体回填(托盘切预设、以后 AI 换壁纸都走它)。
    ///
    /// 跟 `set_values` 的区别:这个连「选中的是哪张卡片、卡片标题、摘要、玻璃配色」一起换,
    /// 所以面板不会出现"卡片还是旧的那张、参数已经是新的"这种中间态。
    pub fn refresh(&self, state: PanelState) {
        let weak = self.weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(panel) = weak.upgrade() {
                apply_state(&panel, &state);
            }
        });
    }

    /// 等面板线程结束(关窗后调用)。
    pub fn wait(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// 打开面板。返回时窗口已经建好,事件循环跑在独立线程上。
pub fn open(state: PanelState, events: Sender<UiEvent>) -> Result<PanelHandle> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<slint::Weak<Panel>>>();
    let closer = events.clone();

    let thread = std::thread::Builder::new()
        .name("mcwp-ui".to_string())
        .spawn(move || match build(&state, events) {
            Ok(ui) => {
                let _ = ready_tx.send(Ok(ui.as_weak()));
                if let Err(err) = ui.run() {
                    tracing::error!("面板事件循环异常退出:{err}");
                } else {
                    tracing::info!("面板已关闭");
                }
                // 让主循环知道面板没了,可以把它从"打开中"摘掉。
                let _ = closer.send(UiEvent::Closed);
            }
            Err(err) => {
                let _ = ready_tx.send(Err(err));
            }
        })
        .map_err(|err| anyhow!("启动面板线程失败:{err}"))?;

    match ready_rx.recv_timeout(Duration::from_secs(20)) {
        Ok(Ok(weak)) => Ok(PanelHandle {
            weak,
            thread: Some(thread),
        }),
        Ok(Err(err)) => Err(err),
        Err(_) => Err(anyhow!("面板启动超时")),
    }
}

fn build(state: &PanelState, events: Sender<UiEvent>) -> Result<Panel> {
    let ui = Panel::new()?;

    apply_state(&ui, state);

    {
        let tx = events.clone();
        ui.on_send_message(move |text| {
            let _ = tx.send(UiEvent::SendMessage(text.to_string()));
        });
    }

    {
        let tx = events.clone();
        ui.on_select_conversation(move |index| {
            let _ = tx.send(UiEvent::SelectConversation(index.max(0) as usize));
        });
    }

    {
        let tx = events.clone();
        ui.on_new_conversation(move || {
            let _ = tx.send(UiEvent::NewConversation);
        });
    }

    // 左栏会话的右键菜单。四个动作都是「改一个已有的会话」,所以一律带着下标走;
    // 真正的改名 / 删除规则在壁纸侧(名字怎么裁、删掉当前会话落到哪),面板这边不判断。
    {
        let tx = events.clone();
        ui.on_rename_conversation(move |index, name| {
            let _ = tx.send(UiEvent::RenameConversation(
                index.max(0) as usize,
                name.to_string(),
            ));
        });
    }

    {
        let tx = events.clone();
        ui.on_delete_conversation(move |index| {
            let _ = tx.send(UiEvent::DeleteConversation(index.max(0) as usize));
        });
    }

    {
        let tx = events.clone();
        ui.on_toggle_pin_conversation(move |index| {
            let _ = tx.send(UiEvent::TogglePinConversation(index.max(0) as usize));
        });
    }

    {
        let tx = events.clone();
        ui.on_mark_conversation_unread(move |index| {
            let _ = tx.send(UiEvent::MarkConversationUnread(index.max(0) as usize));
        });
    }

    {
        let tx = events.clone();
        ui.on_open_wallpaper(move |index| {
            let _ = tx.send(UiEvent::OpenWallpaper(index.max(0) as usize));
        });
    }

    {
        let tx = events.clone();
        ui.on_refresh_library(move || {
            let _ = tx.send(UiEvent::RefreshLibrary);
        });
    }

    {
        // 库那一格的右键菜单:只转发下标,删谁、删完怎么办是壁纸侧的事
        let tx = events.clone();
        ui.on_delete_wallpaper(move |index| {
            let _ = tx.send(UiEvent::DeleteWallpaper(index.max(0) as usize));
        });
    }

    {
        let tx = events.clone();
        ui.on_adopt_wallpaper(move || {
            let _ = tx.send(UiEvent::AdoptWallpaper);
        });
    }

    {
        let tx = events.clone();
        ui.on_import_wallpaper(move || {
            let _ = tx.send(UiEvent::ImportWallpaper);
        });
    }

    {
        // 预览图点开:只把 id 递出去。解多大、从哪儿解、解不开怎么办,都在壁纸侧。
        let tx = events.clone();
        ui.on_open_photo(move |id| {
            let _ = tx.send(UiEvent::OpenPhoto(id.to_string()));
        });
    }

    {
        let tx = events.clone();
        ui.on_viewer_closed(move || {
            let _ = tx.send(UiEvent::ViewerClosed);
        });
    }

    {
        // 预览图右键「设为壁纸」。和 `OpenPhoto` 一样只递 id:换哪张、换不上怎么办,都在壁纸侧。
        let tx = events.clone();
        ui.on_set_as_wallpaper(move |id| {
            let _ = tx.send(UiEvent::SetWallpaper(id.to_string()));
        });
    }

    {
        // 设置页的「保存」。面板只把四个值原样递出去 —— 校验、落盘、重建客户端都在壁纸侧。
        let tx = events.clone();
        ui.on_save_ai_config(move |base_url, model, api_key, thinking, image_base_url, image_model, image_api_key, video_model| {
            let _ = tx.send(UiEvent::SaveAiConfig {
                base_url: base_url.to_string(),
                model: model.to_string(),
                api_key: api_key.to_string(),
                thinking: ThinkingChoice::from_index(thinking),
                image_base_url: image_base_url.to_string(),
                image_model: image_model.to_string(),
                image_api_key: image_api_key.to_string(),
                video_model: video_model.to_string(),
            });
        });
    }

    {
        // 「做成动态壁纸」:面板只递下标 / 一个"就是当前这张"的意思,
        // 真正要不要跑、跑多久、结果怎么落库都是壁纸侧的事。
        let tx = events.clone();
        ui.on_make_video(move |index| {
            let _ = tx.send(UiEvent::MakeVideo(index.max(0) as usize));
        });
        let tx = events.clone();
        ui.on_make_video_current(move || {
            let _ = tx.send(UiEvent::MakeVideoFromCurrent);
        });
    }

    {
        // 暂停 / 继续:面板只转发一下,作废哪一轮、重新发什么是壁纸侧的事。
        let tx = events.clone();
        ui.on_pause_run(move || {
            let _ = tx.send(UiEvent::PauseRun);
        });
        let tx = events.clone();
        ui.on_resume_run(move || {
            let _ = tx.send(UiEvent::ResumeRun);
        });
    }

    {
        // 无边框窗口没有标题栏,关窗只能靠面板上的那颗玻璃圆钮。
        ui.on_close_requested(|| {
            let _ = slint::quit_event_loop();
        });
    }

    // 窗口建出来之后才有 HWND 可用:`show()` 只是把"显示"排进事件循环,真正的 winit 窗口
    // 是事件循环第一次迭代建的,所以句柄得在里面拿(见 `schedule_glass`)。
    ui.show()?;
    let hwnd = Rc::new(Cell::new(0isize));
    schedule_glass(ui.as_weak(), hwnd.clone(), 8);

    {
        // 拖动标题栏。首选交给 Windows 自己的标题栏拖动:进系统模态移动循环后由系统采样
        // 光标,跟手性最好,还自带贴边吸附,也不用在每次指针事件里搬窗口。
        //
        // 拿不到 winit 窗口(后端被换掉)时才退回手动搬窗口:`WindowDrag` 记下「光标 + 窗口
        // 原点」,之后按屏幕光标位移挪窗口(窗口跟着鼠标走时,窗口内坐标会回到原位,靠它是
        // 拖不动的,必须用屏幕坐标)。
        let drag = Rc::new(RefCell::new(None::<WindowDrag>));
        let weak = ui.as_weak();
        let hwnd = hwnd.clone();

        {
            let drag = drag.clone();
            let weak = weak.clone();
            let hwnd = hwnd.clone();
            ui.on_drag_start(move || {
                // 上一轮万一没收到抬起(拖动被系统接管时事件会断),这里重开一次,
                // 起点一律以这次按下为准 —— 不会拿旧原点算出跳变。
                *drag.borrow_mut() = None;
                if let Some(panel) = weak.upgrade() {
                    let native = panel
                        .window()
                        .with_winit_window(|window| window.drag_window())
                        .is_some();
                    if native {
                        return;
                    }
                }
                // 句柄是事件循环里异步取到的;万一用户手快过它,这里再同步试一次。
                let Some(handle) = panel_hwnd(&weak, &hwnd) else {
                    return;
                };
                *drag.borrow_mut() = WindowDrag::begin(handle);
            });
        }

        {
            let drag = drag.clone();
            ui.on_drag_move(move || {
                if let Some(current) = drag.borrow_mut().as_mut() {
                    current.update();
                }
            });
        }

        ui.on_drag_end(move || {
            *drag.borrow_mut() = None;
        });
    }

    {
        // 顶部安全区右边那三颗窗口键里的两颗。和拖动一样在 UI 线程里直接落到 Win32:
        // 无边框窗口没有系统标题栏,最小化 / 放大都得自己代劳(关窗那颗是 close-requested,
        // 退出事件循环即可,不用碰 Win32)。
        //
        // 「放大」记着自己量过的工作区矩形,所以状态由 `WindowZoom` 持有 —— 它也是面板唯一的
        // 窗口状态,面板本身仍然不存业务状态。
        let zoom = Rc::new(RefCell::new(WindowZoom::default()));
        let weak = ui.as_weak();
        let hwnd = hwnd.clone();

        {
            let weak = weak.clone();
            let hwnd = hwnd.clone();
            ui.on_minimize_requested(move || {
                let Some(handle) = panel_hwnd(&weak, &hwnd) else {
                    return;
                };
                if let Err(err) = minimize(handle) {
                    tracing::warn!("最小化面板失败:{err:?}");
                }
            });
        }

        {
            let zoom = zoom.clone();
            let weak = weak.clone();
            ui.on_toggle_maximize_requested(move || {
                let Some(handle) = panel_hwnd(&weak, &hwnd) else {
                    return;
                };
                match zoom.borrow_mut().toggle(handle) {
                    // 面板只关心"该画放大还是还原",窗口状态的真身在 Win32 那侧。
                    Ok(maximized) => {
                        if let Some(panel) = weak.upgrade() {
                            panel.set_maximized(maximized);
                        }
                    }
                    Err(err) => tracing::warn!("放大 / 还原面板失败:{err:?}"),
                }
            });
        }
    }

    Ok(ui)
}

/// 等窗口真的建出来,再取 HWND 交给 DWM 套圆角。
///
/// 定时器的回调跑在 UI 线程的事件循环里(winit 的 `new_events` 会推进定时器),所以在
/// `run()` 之前注册也能生效。窗口还没建好就退避重试几轮,拿到句柄顺手缓存进 `cell`,
/// 拖动逻辑复用同一个句柄,免得再问一次。
fn schedule_glass(ui: slint::Weak<Panel>, cell: Rc<Cell<isize>>, attempts: u32) {
    if attempts == 0 {
        tracing::warn!("一直没取到面板窗口句柄,DWM 圆角与拖动都会失效");
        return;
    }
    slint::Timer::single_shot(Duration::from_millis(16), move || {
        let Some(panel) = ui.upgrade() else { return };
        let Some(hwnd) = window_hwnd(&panel) else {
            schedule_glass(ui, cell, attempts - 1);
            return;
        };
        cell.set(hwnd);
        if let Err(err) = apply_glass(hwnd) {
            tracing::warn!("套用 DWM 玻璃窗口属性失败:{err:?}");
        }
    });
}

/// 取出窗口的 Win32 句柄。非 Windows 或后端没提供句柄时返回 `None`。
fn window_hwnd(ui: &Panel) -> Option<isize> {
    let handle = ui.window().window_handle();
    match handle.window_handle() {
        Ok(raw) => match raw.as_raw() {
            RawWindowHandle::Win32(win32) => Some(win32.hwnd.get()),
            other => {
                tracing::warn!("面板窗口不是 Win32 句柄:{other:?}");
                None
            }
        },
        Err(err) => {
            tracing::warn!("取面板窗口句柄失败:{err}");
            None
        }
    }
}

/// 拿面板窗口的 HWND:优先用 `schedule_glass` 缓存的那份,缓存还是空的就现取一次。
///
/// 句柄是事件循环里异步取到的(见 `schedule_glass`),用户手快过它时这里再同步试一次 ——
/// 拖动与顶栏那两颗窗口键都靠它。
fn panel_hwnd(weak: &slint::Weak<Panel>, cell: &Rc<Cell<isize>>) -> Option<isize> {
    match cell.get() {
        0 => {
            let handle = weak.upgrade().and_then(|panel| window_hwnd(&panel))?;
            cell.set(handle);
            Some(handle)
        }
        handle => Some(handle),
    }
}

// 「库 id → 缩略图」的一张索引,只活在**面板线程**里。
//
// 会话消息里的配图要在 `apply_session` 里贴出来,而那个函数手上只有会话数据(壁纸库是
// 另一次 `refresh_library` 回填进来的)。两个办法:把库一起塞进每次会话回填(于是每发一条
// 消息都要重传几十张缩略图的裸像素),或者在这里留一份索引。选后者。
//
// 里面存的就是库页网格用的**同一个 `slint::Image` 句柄**:Slint 的 Image 是共享像素的引用
// 语义,clone 一次不会多占一份内存,所以这份索引不增加新的峰值。
thread_local! {
    static THUMBS: RefCell<HashMap<String, slint::Image>> = RefCell::new(HashMap::new());
}

/// 把会话数据写进面板:左栏列表、右栏标题与消息。
fn apply_session(ui: &Panel, session: &SessionState) {
    ui.set_conversations(slint::ModelRc::new(slint::VecModel::from(
        session.conversations.clone(),
    )));
    ui.set_current_conversation(session.current as i32);
    ui.set_session_title(session.title.clone().into());
    // 消息里的配图在这一步和壁纸库的缩略图对上号(见 `library_thumbs`)。
    let bubbles: Vec<ChatBubble> = session
        .messages
        .iter()
        .map(|message| {
            let id = message.photo.to_string();
            let shot = if id.is_empty() {
                None
            } else {
                THUMBS.with(|thumbs| thumbs.borrow().get(&id).cloned())
            };
            ChatBubble {
                mine: message.mine,
                text: message.text.clone(),
                photo_id: message.photo.clone(),
                shot: shot.clone().unwrap_or_default(),
                has_shot: shot.is_some(),
                photo_gone: !id.is_empty() && shot.is_none(),
            }
        })
        .collect();
    ui.set_messages(slint::ModelRc::new(slint::VecModel::from(bubbles)));
    ui.set_message_revision(session.revision);
}

/// 把一份面板状态写进已经建好的面板。初始化与「换了一张壁纸」共用同一段。
fn apply_state(ui: &Panel, state: &PanelState) {
    let colors = glass_colors(&state.palette);
    ui.set_ambient_a(colors.ambient_a());
    ui.set_ambient_b(colors.ambient_b());
    ui.set_ambient_c(colors.ambient_c());
    ui.set_glass_tint(colors.glass_tint());
    ui.set_accent(colors.accent());

    ui.set_wallpaper_title(state.title.clone().into());
    // **先库后会话**:消息里的预览图是从库缩略图那份索引里取的(`THUMBS`),顺序反了的话
    // 面板第一次打开时索引还是空的,历史消息里的配图会全变成"这张已经不在壁纸库里了"。
    apply_library(
        ui,
        state.library.clone(),
        state.library_current.clone(),
        state.library_current_index,
    );
    apply_session(ui, &state.session);
}

/// 把壁纸库写进面板:网格本身 + 当前生效的是哪一张(画成选中态)。
///
/// **缩略图在这一步才装成 `slint::Image`** —— 这个函数跑在面板线程里,而裸像素是
/// 跨线程送过来的(见 `LibraryEntryView`)。装的时候要把像素复制一份给 Slint:
/// `SharedPixelBuffer` 拿的是所有权。
fn apply_library(ui: &Panel, library: Vec<LibraryEntryView>, current: String, current_index: i32) {
    // 当前那张的标题要在把 items 交出去之前取出来(交出去之后就只剩 Slint 那边的模型了)
    let current_title = if current_index >= 0 {
        library
            .get(current_index as usize)
            .map(|entry| entry.title.clone())
            .unwrap_or_default()
    } else {
        String::new()
    };

    let items: Vec<LibraryItem> = library
        .into_iter()
        .map(|entry| {
            let (width, height, ref pixels) = entry.thumb;
            let buffer = slint::SharedPixelBuffer::clone_from_slice(pixels, width, height);
            let thumb = slint::Image::from_rgba8(buffer);
            // 顺手进索引:消息里那条回复的配图靠它贴出来(见 `THUMBS` 的说明)
            THUMBS.with(|thumbs| {
                thumbs
                    .borrow_mut()
                    .insert(entry.id.to_string(), thumb.clone());
            });
            LibraryItem {
                id: entry.id.into(),
                title: entry.title.into(),
                hint: entry.hint.into(),
                summary: entry.summary.into(),
                accent: entry.accent,
                video: entry.video,
                thumb,
            }
        })
        .collect();
    ui.set_library(slint::ModelRc::new(slint::VecModel::from(items)));
    ui.set_library_current(current.into());
    ui.set_library_current_index(current_index);
    ui.set_library_current_title(current_title.into());
}

/// 回填设置页的模型配置。
fn apply_ai(ui: &Panel, ai: &AiConfigState) {
    ui.set_ai_base_url(ai.base_url.clone().into());
    ui.set_ai_model(ai.model.clone().into());
    ui.set_ai_key(ai.api_key.clone().into());
    ui.set_ai_image_base_url(ai.image_base_url.clone().into());
    ui.set_ai_image_model(ai.image_model.clone().into());
    ui.set_ai_image_key(ai.image_api_key.clone().into());
    ui.set_ai_video_model(ai.video_model.clone().into());
    ui.set_ai_thinking(ai.thinking.index());
    ui.set_ai_env_override(ai.env_override);
}
