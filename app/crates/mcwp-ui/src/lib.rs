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
use std::collections::HashSet;
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
    /// 用户点了第 `index` 张预设卡片(`PanelState::presets` 的下标)
    SelectPreset(usize),
    /// 参数被拖动 —— 每一次变化都会发,用来做实时预览
    Values {
        /// 三个场景参数,顺序与 `Preset::params()` 一致
        params: [f32; 3],
        brightness: f32,
        motion: f32,
    },
    /// 用户在输入框里发了一条消息(生图界面)
    SendMessage(String),
    /// 点开了左栏里的某个会话(左栏下标)
    SelectConversation(usize),
    /// 点了「+ 新建对话」
    NewConversation,
    /// 面板切到「壁纸库」页:想要一份最新的库列表
    RefreshLibrary,
    /// 点了壁纸库里的第 `index` 张(`PanelState::library` 的下标)
    OpenWallpaper(usize),
    /// 「存为壁纸」
    Save,
    /// 「恢复默认」
    Reset,
    /// 面板窗口被关掉了
    Closed,
}

/// 打开面板时喂给它的初始状态。
pub struct PanelState {
    pub presets: Vec<PresetItem>,
    pub preset_index: usize,
    pub title: String,
    pub hint: String,
    pub fingerprint: String,
    pub summary: String,
    /// 当前壁纸的调色板(`WallpaperSpec::palette`),玻璃色由它推出来
    pub palette: Vec<String>,
    pub labels: [String; 3],
    pub params: [f32; 3],
    pub brightness: f32,
    pub motion: f32,
    /// 生图界面:左栏历史 + 右栏会话
    pub session: SessionState,
    /// 壁纸库:由壁纸侧扫描库目录得到,面板只负责画(和会话数据同一个道理)
    pub library: Vec<LibraryItem>,
    /// 当前生效的壁纸在库里的 id;空串表示这张还没存进库
    pub library_current: String,
}

/// 生图界面的会话数据:左栏的会话列表 + 当前会话的消息。
///
/// 面板不持有任何状态,所以换会话、发消息都是「Rust 改数据 → 整块回填」。`revision`
/// 每改一次 +1:消息列表靠它把滚动位置钉到底部(内容长高了才需要滚,面板自己看不出来)。
///
/// M3 接上 AI 之前,这些会话只活在内存里,退出即散 —— 真正的对话历史该跟壁纸库一样落盘,
/// 那是 M3 的事。
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
        for (index, preset) in self.presets.iter().enumerate() {
            fields.push((format!("预设{index} 标题"), preset.title.to_string()));
            fields.push((format!("预设{index} 说明"), preset.summary.to_string()));
        }
        // 库里的壁纸名是动态的(AI 生成 / 用户存的),同样要盯缺字
        for (index, item) in self.library.iter().enumerate() {
            fields.push((format!("库{index} 标题"), item.title.to_string()));
            fields.push((format!("库{index} 关键词"), item.hint.to_string()));
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

    /// 从壁纸侧回填参数(「恢复默认」之后用),不会引发再次回调。
    pub fn set_values(&self, params: [f32; 3], brightness: f32, motion: f32) {
        let weak = self.weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(panel) = weak.upgrade() {
                panel.set_param_a(params[0]);
                panel.set_param_b(params[1]);
                panel.set_param_c(params[2]);
                panel.set_brightness(brightness);
                panel.set_motion(motion);
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

    /// 整块回填壁纸库(存了新壁纸、面板切到库页时用)。
    pub fn refresh_library(&self, library: Vec<LibraryItem>, current: String) {
        let weak = self.weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(panel) = weak.upgrade() {
                apply_library(&panel, library, current);
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
        ui.on_select_preset(move |index| {
            let _ = tx.send(UiEvent::SelectPreset(index.max(0) as usize));
        });
    }

    {
        // 回调要读面板当前值,所以这里必须拿 weak,不能把 ui 自己捕获进去(会成环)。
        let weak = ui.as_weak();
        let tx = events.clone();
        ui.on_values_changed(move || {
            if let Some(panel) = weak.upgrade() {
                let _ = tx.send(UiEvent::Values {
                    params: [panel.get_param_a(), panel.get_param_b(), panel.get_param_c()],
                    brightness: panel.get_brightness(),
                    motion: panel.get_motion(),
                });
            }
        });
    }

    {
        let tx = events.clone();
        ui.on_save_requested(move || {
            let _ = tx.send(UiEvent::Save);
        });
    }

    {
        let tx = events.clone();
        ui.on_reset_requested(move || {
            let _ = tx.send(UiEvent::Reset);
        });
    }

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

/// 把会话数据写进面板:左栏列表、右栏标题与消息。
fn apply_session(ui: &Panel, session: &SessionState) {
    ui.set_conversations(slint::ModelRc::new(slint::VecModel::from(
        session.conversations.clone(),
    )));
    ui.set_current_conversation(session.current as i32);
    ui.set_session_title(session.title.clone().into());
    ui.set_messages(slint::ModelRc::new(slint::VecModel::from(
        session.messages.clone(),
    )));
    ui.set_message_revision(session.revision);
}

/// 把一份面板状态写进已经建好的面板。初始化与「托盘切预设」共用同一段。
fn apply_state(ui: &Panel, state: &PanelState) {
    let colors = glass_colors(&state.palette);
    ui.set_ambient_a(colors.ambient_a());
    ui.set_ambient_b(colors.ambient_b());
    ui.set_ambient_c(colors.ambient_c());
    ui.set_glass_tint(colors.glass_tint());
    ui.set_accent(colors.accent());

    ui.set_presets(slint::ModelRc::new(slint::VecModel::from(
        state.presets.clone(),
    )));
    ui.set_current_preset(state.preset_index as i32);
    ui.set_wallpaper_title(state.title.clone().into());
    ui.set_wallpaper_hint(state.hint.clone().into());
    ui.set_fingerprint(state.fingerprint.clone().into());
    ui.set_preset_summary(state.summary.clone().into());
    ui.set_label_a(state.labels[0].clone().into());
    ui.set_label_b(state.labels[1].clone().into());
    ui.set_label_c(state.labels[2].clone().into());
    ui.set_param_a(state.params[0]);
    ui.set_param_b(state.params[1]);
    ui.set_param_c(state.params[2]);
    ui.set_brightness(state.brightness);
    ui.set_motion(state.motion);
    apply_session(ui, &state.session);
    apply_library(ui, state.library.clone(), state.library_current.clone());
}

/// 把壁纸库写进面板:列表本身 + 当前生效的是哪一张(画成选中态)。
fn apply_library(ui: &Panel, library: Vec<LibraryItem>, current: String) {
    ui.set_library(slint::ModelRc::new(slint::VecModel::from(library)));
    ui.set_library_current(current.into());
}
