//! MC 壁纸 · M0 壁纸宿主:把 wgpu 渲染的内置场景挂到桌面壁纸层。
//!
//! 用法:
//! ```text
//! mcwp-wallpaper                          # 常驻运行(默认 60fps)
//!                                         # 托盘常驻:打开面板 / 换预设 / 暂停渲染 / 开机自启 / 退出
//! mcwp-wallpaper --seconds 10             # 跑 10 秒自动退出(自检)
//! mcwp-wallpaper --fps 30 --brightness 0.8
//! mcwp-wallpaper --preset nebula_drift   # 用内置场景预设(rainy_window_city / minimal_paper /
//!                                        # nebula_drift / focus_grid)
//! mcwp-wallpaper --preset list           # 列出全部预设与它们的参数
//! mcwp-wallpaper --spec path/xxx.json    # 用一份 WallpaperSpec 渲染
//! mcwp-wallpaper --dump-spec             # 打印当前生效的 spec 与指纹
//! mcwp-wallpaper --export-presets out/   # 把每个预设导出成示例 spec 文件
//! mcwp-wallpaper --panel                  # 同时打开 Slint 控制面板
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

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use mcwp_platform::desktop::{
    memory_mb, primary_monitor_rect, set_per_monitor_dpi_awareness, AttachMode, HostStyle,
    HostWindow,
};
use mcwp_platform::tray::{self, Tray, TrayAction};
#[cfg(feature = "dev-tools")]
use mcwp_platform::desktop::{candidate_report, invalidate_window, set_gdi_background_paint};
#[cfg(feature = "dev-tools")]
use mcwp_platform::diag;
use mcwp_render::{Renderer, SceneParams};
use mcwp_spec::{Preset, WallpaperSpec};
use tracing_subscriber::EnvFilter;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{GetLastError, SetLastError, ERROR_ALREADY_EXISTS, ERROR_SUCCESS};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE,
};

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

/// 决定这一轮渲染什么:`--spec` 优先,其次 `--preset`,都没有就用默认预设。
fn load_spec(args: &Args) -> Result<WallpaperSpec> {
    if let Some(path) = args.spec_path.as_deref() {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读取 spec 失败:{path}"))?;
        return WallpaperSpec::parse(&text).with_context(|| format!("解析 spec 失败:{path}"));
    }
    match args.preset.as_deref() {
        Some(id) => {
            let preset = Preset::from_id(id)
                .ok_or_else(|| anyhow::anyhow!("未知预设 {id},用 `--preset list` 看全部预设"))?;
            Ok(WallpaperSpec::from_preset(preset))
        }
        None => Ok(WallpaperSpec::from_preset(Preset::DEFAULT)),
    }
}

/// 生图界面的会话骨架 —— M3 接上 AI 之前,先让这条链路是通的。
///
/// 面板不持有任何状态:用户发消息 → `UiEvent::SendMessage` → 这里改内存里的会话 → 整块回填
/// 面板。现在还没接模型,回复位由 `PLACEHOLDER_REPLY` 占着;等 M3 落地,把 `send()` 里那句
/// 占位换成「文本 → WallpaperSpec → 应用」的管线即可,面板侧一行都不用动。
///
/// 会话只活在内存里(退出即散)。真落盘是 M3 的事:对话历史该跟壁纸库放一起(见方案第五节),
/// 而不是塞进面板。
struct Session {
    /// 会话的身份。列表会重排(置顶会把整组挪到前面),面板那侧的下标随时在变,
    /// 内部认人一律靠 id,不靠下标。
    id: u64,
    title: String,
    messages: Vec<mcwp_ui::ChatMessage>,
    /// 置顶的排在列表最前面
    pinned: bool,
    /// 有未读:行尾挂个圆点,切进这个会话就清掉
    unread: bool,
}

struct Sessions {
    /// 显示顺序:置顶的一组在前,两组各自保持创建先后
    items: Vec<Session>,
    /// 当前会话的 **id**(不是下标 —— 置顶会重排)
    active: u64,
    /// 下一个会话的 id
    next_id: u64,
    /// 消息版本号,每改一次 +1:面板靠它把消息列表钉到底部
    revision: i32,
}

/// 还没接 AI 时的回复占位。故意写清楚"这是占位",免得看着像模型真的回了话。
const PLACEHOLDER_REPLY: &str = "(AI 未接入 · M3)已记下。接上模型后,这里会给出按这句话生成的壁纸。";

/// 新会话的名字。第一条消息会把它顶掉(见 `title_from`)。
const NEW_SESSION_TITLE: &str = "新的对话";

impl Sessions {
    fn new() -> Self {
        let mut sessions = Self {
            items: Vec::new(),
            active: 0,
            next_id: 1,
            revision: 0,
        };
        sessions.create();
        // `create` 那一下的 +1 不算:面板刚拿到手,不需要为"钉到底部"再滚一次
        sessions.revision = 0;
        sessions
    }

    /// 当前会话在列表里的下标(面板要的是下标,内部存的是 id)。
    ///
    /// 兜底给 0:删除那边保证了列表不空、`active` 一定指得到人,这里只是不给 panic 留机会。
    fn active_index(&self) -> usize {
        self.items
            .iter()
            .position(|session| session.id == self.active)
            .unwrap_or(0)
    }

    /// 交给面板的一整块会话数据。
    fn state(&self) -> mcwp_ui::SessionState {
        let active = self.active_index();
        mcwp_ui::SessionState {
            conversations: self
                .items
                .iter()
                .map(|session| mcwp_ui::ConversationItem {
                    heading: session.title.clone().into(),
                    sub: Self::preview(session).into(),
                    pinned: session.pinned,
                    unread: session.unread,
                })
                .collect(),
            current: active,
            title: self.items[active].title.clone(),
            messages: self.items[active].messages.clone(),
            revision: self.revision,
        }
    }

    /// 左栏那一行小字:最后一条消息的开头。
    fn preview(session: &Session) -> String {
        let Some(last) = session.messages.last() else {
            return "还没有消息".to_string();
        };
        let text = last.text.to_string();
        if text.chars().count() > 16 {
            format!("{}…", text.chars().take(16).collect::<String>())
        } else {
            text
        }
    }

    /// 记一条用户消息 + 一条占位回复;第一条消息顺手给会话起个名。
    fn send(&mut self, text: &str) {
        let active = self.active_index();
        let session = &mut self.items[active];
        if session.messages.is_empty() {
            session.title = title_from(text);
        }
        session.messages.push(mcwp_ui::ChatMessage {
            mine: true,
            text: text.into(),
        });
        session.messages.push(mcwp_ui::ChatMessage {
            mine: false,
            text: PLACEHOLDER_REPLY.into(),
        });
        // 自己刚说过话的会话不该挂着未读点
        session.unread = false;
        self.revision += 1;
    }

    /// 切到左栏的第 `index` 个会话。切过去就当作读过了。
    fn select(&mut self, index: usize) {
        let Some(session) = self.items.get_mut(index) else {
            return;
        };
        let id = session.id;
        session.unread = false;
        if id != self.active {
            self.active = id;
        }
        self.revision += 1;
    }

    /// 新建一个空会话并切过去。
    fn create(&mut self) {
        let id = self.next_id;
        self.next_id += 1;
        self.items.push(Session {
            id,
            title: NEW_SESSION_TITLE.to_string(),
            messages: Vec::new(),
            pinned: false,
            unread: false,
        });
        self.active = id;
        self.revision += 1;
    }

    /// 改名。名字两端空白清掉;清完是空的当作没改 —— 名字是列表里唯一的抓手,不能空。
    fn rename(&mut self, index: usize, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let Some(session) = self.items.get_mut(index) else {
            return;
        };
        if session.title == name {
            return;
        }
        session.title = name.to_string();
        self.revision += 1;
    }

    /// 删掉第 `index` 个会话。删的是当前会话就落到顶上来的那一条(没有就往前退一格);
    /// 一条都不剩时补一个空会话 —— 右栏不该为"列表空了"再写一套空状态。
    fn remove(&mut self, index: usize) {
        if index >= self.items.len() {
            return;
        }
        let removed = self.items.remove(index);
        if self.items.is_empty() {
            self.create();
        } else if removed.id == self.active {
            let next = index.min(self.items.len() - 1);
            self.active = self.items[next].id;
        }
        self.revision += 1;
    }

    /// 置顶 / 取消置顶,返回改完之后是不是置顶状态(面板拿它写状态行)。
    ///
    /// 置顶的整组挪到最前面,两组各自保持原来的先后 —— `sort_by_key` 是稳定排序,
    /// 所以只按"是不是置顶"排,组内的先后不会乱。
    fn toggle_pin(&mut self, index: usize) -> bool {
        let Some(session) = self.items.get_mut(index) else {
            return false;
        };
        session.pinned = !session.pinned;
        let pinned = session.pinned;
        self.items.sort_by_key(|session| !session.pinned);
        self.revision += 1;
        pinned
    }

    /// 标为未读。故意不切过去、也不动当前会话:它就是个"回头再看"的记号。
    fn mark_unread(&mut self, index: usize) {
        let Some(session) = self.items.get_mut(index) else {
            return;
        };
        if !session.unread {
            session.unread = true;
            self.revision += 1;
        }
    }
}

/// 用第一条消息给会话起名:多空白折成一个空格,掐头 12 个字。
fn title_from(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 12 {
        format!("{}…", flat.chars().take(12).collect::<String>())
    } else {
        flat
    }
}

/// 把「当前 spec + 各预设默认值 + 壁纸库 + 会话」拼成面板的初始状态。
fn panel_state(
    spec: &WallpaperSpec,
    brightness: f32,
    sessions: &Sessions,
    library: &[LibraryEntry],
) -> mcwp_ui::PanelState {
    // 每张卡片自带它的三个参数名与默认值,这样点卡片时面板不用等 Rust 回话就能先自己更新。
    let presets = mcwp_spec::PRESETS
        .iter()
        .map(|preset| {
            let slots = preset.params();
            let defaults = WallpaperSpec::from_preset(*preset);
            mcwp_ui::PresetItem {
                id: preset.id().into(),
                title: preset.title().into(),
                summary: preset.summary().into(),
                label_a: slots[0].label.into(),
                label_b: slots[1].label.into(),
                label_c: slots[2].label.into(),
                value_a: defaults.param(slots[0].key),
                value_b: defaults.param(slots[1].key),
                value_c: defaults.param(slots[2].key),
                motion: defaults.motion.intensity,
            }
        })
        .collect();

    let preset = spec.preset();
    let preset_index = mcwp_spec::PRESETS
        .iter()
        .position(|candidate| *candidate == preset)
        .unwrap_or(0);
    let slots = preset.params();

    mcwp_ui::PanelState {
        presets,
        preset_index,
        title: spec.title.clone(),
        hint: spec.keyword_hint(),
        // 面板上那条「指纹」用库里的身份口径(归一化后的 hash):和存进库时的文件名、
        // 壁纸库页里那串字符都是同一个 —— 同一张壁纸到处都念得通。
        fingerprint: library_id(spec),
        summary: preset.summary().to_string(),
        palette: spec.palette.clone(),
        labels: [
            slots[0].label.to_string(),
            slots[1].label.to_string(),
            slots[2].label.to_string(),
        ],
        params: [
            spec.param(slots[0].key),
            spec.param(slots[1].key),
            spec.param(slots[2].key),
        ],
        brightness,
        motion: spec.motion.intensity,
        session: sessions.state(),
        library: library_items(library),
        library_current: library_current(spec, library),
    }
}

/// 打开控制面板:把「当前 spec + 预设表」拼成初始状态交给 UI 线程。
///
/// `--panel` 启动时和托盘「打开控制面板」都走这里。
fn open_panel(
    spec: &WallpaperSpec,
    brightness: f32,
    sessions: &Sessions,
    library: &[LibraryEntry],
    events: &Sender<mcwp_ui::UiEvent>,
) -> Option<mcwp_ui::PanelHandle> {
    let state = panel_state(spec, brightness, sessions, library);
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

/// 壁纸库目录:`%APPDATA%\MC壁纸\library`,一张壁纸 = 一个 JSON,文件名就是 spec 指纹。
fn library_root() -> Result<PathBuf> {
    Ok(std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("读不到 APPDATA,无法定位壁纸库目录"))?
        .join("MC壁纸")
        .join("library"))
}

/// 一份 spec 在壁纸库里的 id(也就是文件名)。
///
/// `id` 自己不参与指纹计算,所以先把 source 归一成 Snapshot、算完 hash 再当作文件名 ——
/// 反复存同一份壁纸不会冒出新条目,存与读也用同一把尺子。
fn library_id(spec: &WallpaperSpec) -> String {
    let mut stored = spec.clone();
    stored.source = mcwp_spec::Source::Snapshot;
    stored.normalize();
    stored.id = stored.spec_hash();
    stored.id
}

/// 存进壁纸库:一张壁纸 = 一个 JSON,文件名就是 spec 指纹,所以重复保存天然去重。
///
/// 这是壁纸库的第一个版本(文件版);等要做标签、历史、缩略图时再换 SQLite。
fn save_to_library(spec: &WallpaperSpec) -> Result<PathBuf> {
    let root = library_root()?;
    std::fs::create_dir_all(&root)
        .with_context(|| format!("创建壁纸库目录失败:{}", root.display()))?;

    // 落盘的那份是"快照":source 钉成 Snapshot,文件名用归一化后的指纹。
    let mut stored = spec.clone();
    stored.source = mcwp_spec::Source::Snapshot;
    stored.normalize();
    stored.id = library_id(spec);

    let path = root.join(format!("{}.json", stored.id));
    std::fs::write(&path, stored.to_pretty_json())
        .with_context(|| format!("写入壁纸失败:{}", path.display()))?;
    Ok(path)
}

/// 壁纸库里的一个条目:面板要显示的那份(纯数据)+ spec 真身(点它的时候要切过去)。
struct LibraryEntry {
    item: mcwp_ui::LibraryItem,
    spec: WallpaperSpec,
}

/// 扫一遍壁纸库目录,最近存的排最前。
///
/// 面板不碰文件系统:库列表的真相在壁纸这一侧,面板只是它的投影 —— 和会话数据一个道理。
/// 库目录是用户可写的地方(手工删文件、拷别人的壁纸进去都算正常用法),所以坏 JSON 只记一条
/// 日志跳过,不能因为它让整块面板打不开。
fn scan_library() -> Vec<LibraryEntry> {
    let root = match library_root() {
        Ok(root) => root,
        Err(err) => {
            tracing::warn!("扫壁纸库失败:{err}");
            return Vec::new();
        }
    };
    // 一次都没存过时目录还不存在,这是正常状态,不是错误。
    let Ok(dir) = std::fs::read_dir(&root) else {
        return Vec::new();
    };

    let mut files: Vec<(std::time::SystemTime, PathBuf)> = dir
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| {
            let path = entry.path();
            let stamp = entry.metadata().ok()?.modified().ok()?;
            Some((stamp, path))
        })
        .collect();
    files.sort_by(|left, right| right.0.cmp(&left.0));

    files
        .into_iter()
        .filter_map(|(_, path)| match std::fs::read_to_string(&path) {
            Ok(text) => match WallpaperSpec::parse(&text) {
                Ok(spec) => {
                    // 每张壁纸的卡片用自己调色板里的主色,一眼能认出是哪张。
                    let accent = mcwp_ui::glass_colors(&spec.palette).accent();
                    Some(LibraryEntry {
                        item: mcwp_ui::LibraryItem {
                            // 面板拿它与"当前壁纸"比对,所以用归一化后的指纹当身份 ——
                            // 手工拷进来的文件(没有 id 字段)也算得出来,点一下就认识它了。
                            id: library_id(&spec).into(),
                            title: spec.title.clone().into(),
                            hint: spec.keyword_hint().into(),
                            summary: spec.preset().summary().into(),
                            accent,
                        },
                        spec,
                    })
                }
                Err(err) => {
                    tracing::warn!("壁纸库里的 {} 解析失败,跳过:{err}", path.display());
                    None
                }
            },
            Err(err) => {
                tracing::warn!("读壁纸库里的 {} 失败,跳过:{err}", path.display());
                None
            }
        })
        .collect()
}

/// 交给面板的那份库列表(只有显示数据,spec 真身留在这一侧)。
fn library_items(library: &[LibraryEntry]) -> Vec<mcwp_ui::LibraryItem> {
    library.iter().map(|entry| entry.item.clone()).collect()
}

/// 当前生效的壁纸在库里的 id;不在库里(比如刚用内置预设改了参数、还没存过)就是空串。
fn library_current(spec: &WallpaperSpec, library: &[LibraryEntry]) -> String {
    let id = library_id(spec);
    library
        .iter()
        .any(|entry| entry.item.id == id)
        .then_some(id)
        .unwrap_or_default()
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
    /// 直接指定一份 spec 文件
    spec_path: Option<String>,
    /// 用内置预设;`list` 表示只列目录
    preset: Option<String>,
    /// 打印当前生效的 spec
    dump_spec: bool,
    /// 把全部预设导出成示例 spec 到指定目录
    export_presets: Option<String>,
    /// 启动时打开控制面板
    panel: bool,
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
            spec_path: None,
            preset: None,
            dump_spec: false,
            export_presets: None,
            panel: false,
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
                "--spec" => {
                    args.spec_path = value.cloned();
                    i += 2;
                }
                "--preset" => {
                    args.preset = value.cloned();
                    i += 2;
                }
                "--dump-spec" => {
                    args.dump_spec = true;
                    i += 1;
                }
                "--export-presets" => {
                    args.export_presets = value.cloned();
                    i += 2;
                }
                "--panel" => {
                    args.panel = true;
                    i += 1;
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

    // 「列目录」和「导出示例」是一次性动作,处理完直接退出,不建窗口也不建渲染器。
    if args.preset.as_deref() == Some("list") {
        print!("{}", mcwp_spec::presets::catalog());
        return Ok(());
    }
    if let Some(dir) = args.export_presets.as_deref() {
        std::fs::create_dir_all(dir).with_context(|| format!("创建目录失败:{dir}"))?;
        for preset in mcwp_spec::PRESETS {
            let spec = WallpaperSpec::from_preset(preset);
            let path = std::path::Path::new(dir).join(format!("{}.json", preset.id()));
            std::fs::write(&path, spec.to_pretty_json())
                .with_context(|| format!("写入失败:{}", path.display()))?;
            tracing::info!("导出预设 {} → {}", preset.id(), path.display());
        }
        return Ok(());
    }

    // 排查用的那几个参数要么不挂桌面要么只是看状态,不该被单实例挡住。
    let is_diagnostic_run = args.standalone_window
        || args.plain_window
        || args.dump
        || args.gdi_only
        || args.selftest;
    if !is_diagnostic_run && !claim_single_instance() {
        return Ok(());
    }

    let mut spec = load_spec(&args)?;
    tracing::info!(
        "壁纸规格:{} | 场景={} | 关键词={} | 指纹={}",
        spec.title,
        spec.preset().id(),
        spec.keyword_hint(),
        spec.spec_hash()
    );
    if args.dump_spec {
        println!("{}", spec.to_pretty_json());
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

    let mut brightness = args.brightness.clamp(0.2, 1.6);
    let mut params = SceneParams::from_spec(&spec, width as u32, height as u32, brightness);

    #[cfg(feature = "dev-tools")]
    if args.selftest {
        params.time = 1.0;
        let pixel = renderer.readback_center(&params)?;
        println!("离屏回读中心像素 = {pixel:?}(洋红=(255,0,255) 表示绘制没发生)");
        return Ok(());
    }

    // —— 控制面板 ——
    // 面板跑在自己的线程和自己的 Slint 事件循环里;这里只保留一个事件队列,
    // 每帧开头把它清空一次,所以拖动滑杆的下一帧就能看到变化。
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<mcwp_ui::UiEvent>();
    // 会话数据在壁纸这一侧(Rust),面板只是它的投影;面板关掉再开,历史还在。
    let mut sessions = Sessions::new();
    // 壁纸库同理:开面板时先扫一遍库目录,面板打开就是最新的一张列表。
    let mut library = scan_library();
    tracing::info!("壁纸库里有 {} 张壁纸", library.len());
    let mut panel = if args.panel {
        open_panel(&spec, brightness, &sessions, &library, &ui_tx)
    } else {
        None
    };

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
    // 当前选中的预设下标。「换一个预设」要按它往后轮。
    let mut preset_index = mcwp_spec::PRESETS
        .iter()
        .position(|candidate| *candidate == spec.preset())
        .unwrap_or(0);
    let mut paused = false;
    // 动画时钟按帧累加,而不是直接取墙钟 —— 暂停几分钟再继续,动画从停下的地方接着走。
    let mut clock = 0.0f32;
    let mut last_tick = Instant::now();

    while window.pump_pending() {
        // 这一帧要切到哪张预设:面板和托盘两个来源都往这里投票,循环末尾统一落地。
        let mut wanted_preset: Option<usize> = None;
        while let Ok(event) = ui_rx.try_recv() {
            tracing::debug!("面板事件:{event:?}");
            match event {
                mcwp_ui::UiEvent::SelectPreset(index) => wanted_preset = Some(index),
                mcwp_ui::UiEvent::Values {
                    params: ui_params,
                    brightness: ui_brightness,
                    motion,
                } => {
                    // 面板上的三个滑杆就是该预设声明的三个参数,按位置写回 spec。
                    for (spec_slot, value) in spec.preset().params().iter().zip(ui_params) {
                        spec.scene.params.insert(spec_slot.key.to_string(), value);
                    }
                    spec.motion.intensity = motion;
                    spec.normalize();
                    brightness = ui_brightness.clamp(0.2, 1.6);
                    params = SceneParams::from_spec(&spec, width as u32, height as u32, brightness);
                }
                // —— 生图界面:Rust 侧先记着,模型那一环等 M3 —— 
                mcwp_ui::UiEvent::SendMessage(text) => {
                    if text.trim().is_empty() {
                        continue;
                    }
                    sessions.send(&text);
                    // 用户敲的字面大概率超出内嵌字体子集(生僻字、emoji),缺字是静默不画,先记一笔。
                    let missing = mcwp_ui::missing_chars(&text);
                    if !missing.is_empty() {
                        tracing::warn!(
                            "这条消息里有 {} 个字符不在内嵌字体内:{missing:?}",
                            missing.len()
                        );
                    }
                    if let Some(handle) = &panel {
                        handle.refresh_session(sessions.state());
                        handle.set_status("AI 未接入 · 这句已经记在当前会话里");
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
                        handle.set_status(if pinned { "已置顶" } else { "已取消置顶" });
                    }
                }
                mcwp_ui::UiEvent::MarkConversationUnread(index) => {
                    sessions.mark_unread(index);
                    if let Some(handle) = &panel {
                        handle.refresh_session(sessions.state());
                        handle.set_status("已标为未读");
                    }
                }
                mcwp_ui::UiEvent::Save => match save_to_library(&spec) {
                    Ok(path) => {
                        let name = path
                            .file_name()
                            .map(|name| name.to_string_lossy().to_string())
                            .unwrap_or_default();
                        tracing::info!("已存为壁纸:{}", path.display());
                        // 库里多了一张:顺手重扫一遍灌回面板,不用等下次开面板。
                        library = scan_library();
                        if let Some(panel) = &panel {
                            panel.refresh_library(
                                library_items(&library),
                                library_current(&spec, &library),
                            );
                            panel.set_status(format!("已存为壁纸 · {name}"));
                        }
                    }
                    Err(err) => {
                        tracing::error!("存为壁纸失败:{err:?}");
                        if let Some(panel) = &panel {
                            panel.set_status(format!("保存失败:{err}"));
                        }
                    }
                },
                mcwp_ui::UiEvent::Reset => {
                    spec = WallpaperSpec::from_preset(spec.preset());
                    params = SceneParams::from_spec(&spec, width as u32, height as u32, brightness);
                    if let Some(panel) = &panel {
                        panel.set_values(
                            [
                                spec.param(spec.preset().params()[0].key),
                                spec.param(spec.preset().params()[1].key),
                                spec.param(spec.preset().params()[2].key),
                            ],
                            brightness,
                            spec.motion.intensity,
                        );
                        panel.set_status("已恢复默认");
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
                        handle.refresh_library(
                            library_items(&library),
                            library_current(&spec, &library),
                        );
                    }
                }
                // 点库里的某一张:直接换成它,和托盘切预设走同一套落地逻辑。
                mcwp_ui::UiEvent::OpenWallpaper(index) => {
                    if let Some(entry) = library.get(index) {
                        spec = entry.spec.clone();
                        params = SceneParams::from_spec(&spec, width as u32, height as u32, brightness);
                        preset_index = mcwp_spec::PRESETS
                            .iter()
                            .position(|candidate| *candidate == spec.preset())
                            .unwrap_or(0);
                        tracing::info!("切换到壁纸库:{}({})", entry.item.title, entry.item.id);
                        if let Some(handle) = &panel {
                            handle.refresh(panel_state(&spec, brightness, &sessions, &library));
                            handle.set_status(format!("已切到 {}", entry.item.title));
                        }
                    }
                }
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
                        panel = open_panel(&spec, brightness, &sessions, &library, &ui_tx);
                    }
                }
                TrayAction::NextPreset => {
                    let count = mcwp_spec::PRESETS.len();
                    if count > 0 {
                        wanted_preset = Some((preset_index + 1) % count);
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

        if let Some(index) = wanted_preset {
            if let Some(preset) = mcwp_spec::PRESETS.get(index) {
                spec = WallpaperSpec::from_preset(*preset);
                params = SceneParams::from_spec(&spec, width as u32, height as u32, brightness);
                preset_index = index;
                if let Some(handle) = &panel {
                    // 面板自己点卡片时已经先改过自己了,这次回填是无副作用的对齐;
                    // 托盘切换则全靠它把面板同步过来。
                    handle.refresh(panel_state(&spec, brightness, &sessions, &library));
                }
                tracing::info!("切换到预设:{}", preset.id());
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
            clock += (frame_start - last_tick).as_secs_f32();
            params.time = clock;
            renderer.render(&params)?;
            frames_in_window += 1;
            rendered_total += 1;
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

    tracing::info!("退出,共渲染 {rendered_total} 帧,跳过 {} 帧", renderer.skipped_frames);
    Ok(())
}
