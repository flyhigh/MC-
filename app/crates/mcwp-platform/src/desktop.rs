//! Windows 桌面层:创建壁纸宿主窗口,并挂到桌面图标之下的「壁纸层」。
//!
//! Windows 各版本的桌面窗口结构不一样,所以这里把已知结构都实现了一遍,
//! 由 [`AttachMode`] 选择,`auto` 会先探测当前系统再决定。
//!
//! 结构 A(经典,Win7 ~ Win10 及部分 Win11):
//! ```text
//! Progman
//! WorkerW                <- 持有 SHELLDLL_DefView 的顶层窗口
//!   └─ SHELLDLL_DefView
//!        └─ SysListView32(桌面图标)
//! WorkerW                <- 背景层,壁纸窗口挂这里
//! ```
//!
//! 结构 B(`SHELLDLL_DefView` 直接在 `Progman` 里,顶层没有可用的 `WorkerW`):
//! 只能挂到 `Progman`,并把自己压在 `SHELLDLL_DefView` 下面(否则会盖住图标)。
//!
//! 结构 C(`SHELLDLL_DefView` 在 `Progman` 里,但顶层 `WorkerW` 存在却
//! `IsWindowVisible=false`):Win11 24H2 之后实测到的形态,需要显式 `ShowWindow`。

use std::cell::RefCell;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{anyhow, Result};
use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, GetMonitorInfoW, InvalidateRect, MonitorFromPoint, ScreenToClient,
    ValidateRect, MONITORINFO, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EnumWindows, FindWindowExW,
    FindWindowW, GetAncestor, GetClassNameW, GetSystemMetrics, PeekMessageW, PostQuitMessage,
    RegisterClassExW, GA_ROOT,
    SendMessageTimeoutW, SetParent, SetWindowPos, ShowWindow, TranslateMessage,
    HWND_BOTTOM, HWND_TOP, MSG, PM_REMOVE, SMTO_NORMAL, SW_SHOWNOACTIVATE, SWP_NOACTIVATE,
    SWP_SHOWWINDOW,
    SYSTEM_METRICS_INDEX, WM_DESTROY, WM_ERASEBKGND, WM_PAINT, WM_QUIT, WNDCLASSEXW,
    WINDOW_EX_STYLE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_OVERLAPPEDWINDOW, WS_POPUP,
};

// 诊断模块只在 `dev-tools` 下编译;正式路径用下面这两个最小助手。
#[cfg(feature = "dev-tools")]
use crate::diag;

/// 窗口类名,挂载策略靠它区分 Progman / WorkerW / SHELLDLL_DefView。
fn window_class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    if n <= 0 {
        String::new()
    } else {
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

/// 顶层祖先窗口。
fn window_root(hwnd: HWND) -> HWND {
    unsafe { GetAncestor(hwnd, GA_ROOT) }
}

/// 桌面私有消息:让 Progman 生成/整理 WorkerW(微软未公开,2009 年起一直可用)。
const WM_SPAWN_WORKER: u32 = 0x052C;

const HOST_CLASS: PCWSTR = w!("MCWP_WallpaperHost");

/// 诊断开关:让 `WM_ERASEBKGND` / `WM_PAINT` 走默认处理,用类的背景刷填充窗口。
/// 用来区分「窗口没被合成」和「DXGI 呈现没生效」。
static PAINT_BACKGROUND: AtomicBool = AtomicBool::new(false);

pub fn set_gdi_background_paint(enabled: bool) {
    PAINT_BACKGROUND.store(enabled, Ordering::Relaxed);
}

/// 让窗口重绘一次(诊断用)。
pub fn invalidate_window(hwnd: HWND) {
    let _ = unsafe { InvalidateRect(Some(hwnd), None, true) };
}

/// 当前进程内存快照 `(工作集 MB, 私有提交 MB)`,用于跟踪「常驻内存 ≤ 100MB」这条指标。
pub fn memory_mb() -> (f64, f64) {
    use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;

    let mut counters = PROCESS_MEMORY_COUNTERS::default();
    let ok = unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    };
    if ok.as_bool() {
        (
            counters.WorkingSetSize as f64 / 1_048_576.0,
            counters.PagefileUsage as f64 / 1_048_576.0,
        )
    } else {
        (0.0, 0.0)
    }
}

/// 显示器矩形 `(x, y, width, height)`。
pub type Rect = (i32, i32, i32, i32);

/// 开启 Per-Monitor V2 DPI 感知。
///
/// 必须在创建任何窗口之前调用:否则在多显示器 + 高缩放环境下,
/// 系统会把我们请求的分辨率虚拟化成别的尺寸、并把窗口挪到别的显示器上。
pub fn set_per_monitor_dpi_awareness() -> bool {
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_ok() }
}

/// 取主显示器矩形(多屏时代只需改这里换 `EnumDisplayMonitors`)。
pub fn primary_monitor_rect() -> Rect {
    unsafe {
        let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            let r = info.rcMonitor;
            (r.left, r.top, r.right - r.left, r.bottom - r.top)
        } else {
            (
                0,
                0,
                GetSystemMetrics(SYSTEM_METRICS_INDEX(0)),
                GetSystemMetrics(SYSTEM_METRICS_INDEX(1)),
            )
        }
    }
}

/// 壁纸层挂载策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachMode {
    /// 先探测当前系统的桌面结构再决定
    Auto,
    /// 挂 `Progman`,并压到 `SHELLDLL_DefView` 之下(不挡图标)
    BelowDefView,
    /// 挂 `Progman`,但不改 Z 序
    Progman,
    /// 挂 `Progman`,并把自己沉到最底
    ProgmanBottom,
    /// 挂 `Progman` 的子 `WorkerW`
    ProgmanWorker,
    /// 经典逻辑:挂到持有 `SHELLDLL_DefView` 的顶层窗口之后那个 `WorkerW`
    Workerw,
}

impl AttachMode {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "auto" => Some(Self::Auto),
            "below-defview" => Some(Self::BelowDefView),
            "progman" => Some(Self::Progman),
            "progman-bottom" => Some(Self::ProgmanBottom),
            "progman-worker" => Some(Self::ProgmanWorker),
            "workerw" | "classic" => Some(Self::Workerw),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::BelowDefView => "below-defview",
            Self::Progman => "progman",
            Self::ProgmanBottom => "progman-bottom",
            Self::ProgmanWorker => "progman-worker",
            Self::Workerw => "workerw",
        }
    }

    /// 供 `--attach` 提示用
    pub const NAMES: &'static str =
        "auto / below-defview / progman / progman-bottom / progman-worker / workerw";
}

/// Z 序落点。
#[derive(Debug, Clone, Copy)]
enum ZPlace {
    Top,
    Bottom,
    Below(HWND),
}

/// 挂载目标 + Z 序落点。
struct Host {
    hwnd: HWND,
    z: ZPlace,
    desc: String,
}

fn non_invalid(hwnd: HWND) -> Option<HWND> {
    (!hwnd.0.is_null()).then_some(hwnd)
}

fn spawn_workerw() {
    unsafe {
        let progman = progman();
        if !progman.0.is_null() {
            let _ = SendMessageTimeoutW(
                progman,
                WM_SPAWN_WORKER,
                WPARAM(0),
                LPARAM(0),
                SMTO_NORMAL,
                1000,
                None,
            );
        }
    }
}

pub fn progman() -> HWND {
    unsafe { FindWindowW(w!("Progman"), PCWSTR::null()).unwrap_or_default() }
}

/// 桌面图标层 `SHELLDLL_DefView`(可能挂在 `Progman` 里,也可能挂在某个顶层 `WorkerW` 里)。
pub fn defview() -> Option<HWND> {
    unsafe {
        let top = FindWindowW(w!("SHELLDLL_DefView"), PCWSTR::null()).unwrap_or_default();
        if let Some(h) = non_invalid(top) {
            return Some(h);
        }
        let progman = progman();
        if let Some(pm) = non_invalid(progman) {
            if let Ok(h) = FindWindowExW(Some(pm), None, w!("SHELLDLL_DefView"), PCWSTR::null()) {
                return non_invalid(h);
            }
        }
        None
    }
}

thread_local! {
    static FOUND: RefCell<Vec<HWND>> = const { RefCell::new(Vec::new()) };
}

unsafe extern "system" fn enum_class_filter(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let wanted = lparam.0 as *const u16;
    let wanted = PCWSTR(wanted);
    let name = window_class_name(hwnd);
    let want = unsafe { wanted.to_string().unwrap_or_default() };
    if name == want {
        FOUND.with(|f| f.borrow_mut().push(hwnd));
    }
    BOOL(1)
}

fn top_level_of_class(class: &[u16]) -> Vec<HWND> {
    FOUND.with(|f| f.borrow_mut().clear());
    unsafe {
        let _ = EnumWindows(Some(enum_class_filter), LPARAM(class.as_ptr() as isize));
    }
    FOUND.with(|f| f.borrow().clone())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 全部顶层 `WorkerW`(含不可见的)。
pub fn top_worker_windows() -> Vec<HWND> {
    top_level_of_class(&wide("WorkerW"))
}

/// `Progman` 的子 `WorkerW`。
pub fn progman_child_worker() -> Option<HWND> {
    let progman = non_invalid(progman())?;
    unsafe { FindWindowExW(Some(progman), None, w!("WorkerW"), PCWSTR::null()) }
        .ok()
        .and_then(non_invalid)
}

/// 窗口尺寸(像素)。
fn window_size(hwnd: HWND) -> (i32, i32) {
    let mut r = windows::Win32::Foundation::RECT::default();
    if unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut r) }.is_err() {
        return (0, 0);
    }
    (r.right - r.left, r.bottom - r.top)
}

/// 一个窗口是否铺满了指定显示器(用来把 IME 宿主那种 136x39 的小 `WorkerW` 排除掉)。
fn covers_monitor(hwnd: HWND, monitor: Rect) -> bool {
    let (w, h) = window_size(hwnd);
    w >= monitor.2 && h >= monitor.3
}

/// Win11 的壁纸层:`Progman` 里那个铺满屏幕、`WS_DISABLED | WS_EX_TRANSPARENT` 的子 `WorkerW`。
pub fn progman_background_worker() -> Option<HWND> {
    let progman = non_invalid(progman())?;
    let monitor = primary_monitor_rect();
    let mut best: Option<HWND> = None;
    unsafe {
        let mut child = FindWindowExW(Some(progman), None, w!("WorkerW"), PCWSTR::null())
            .ok()
            .and_then(non_invalid);
        while let Some(hwnd) = child {
            if covers_monitor(hwnd, monitor) {
                best = Some(hwnd);
                break;
            }
            child = FindWindowExW(Some(progman), Some(hwnd), w!("WorkerW"), PCWSTR::null())
                .ok()
                .and_then(non_invalid);
        }
    }
    best
}

/// 经典布局:持有 `SHELLDLL_DefView` 的顶层窗口的**下一个同级** `WorkerW`(背景层)。
pub fn classic_background_worker() -> Option<HWND> {
    unsafe {
        let mut found: Option<HWND> = None;
        for w in top_worker_windows() {
            if let Ok(dv) = FindWindowExW(Some(w), None, w!("SHELLDLL_DefView"), PCWSTR::null()) {
                if !dv.0.is_null() {
                    found = FindWindowExW(None, Some(w), w!("WorkerW"), PCWSTR::null())
                        .ok()
                        .and_then(non_invalid);
                    break;
                }
            }
        }
        if found.is_some() {
            return found;
        }
        // Win11:DefView 在 Progman 里,那么背景层就是 Progman 之后的那个 WorkerW
        if let Some(pm) = non_invalid(progman()) {
            if let Some(dv) = defview() {
                let dv_root = window_root(dv);
                if dv_root.0 == pm.0 {
                    // 注意:系统里还有一堆 136x39 的 IME 宿主也叫 WorkerW,必须按尺寸过滤,
                    // 否则会把壁纸窗口挂到那种看不见的小窗口里(踩过一次)。
                    let monitor = primary_monitor_rect();
                    let candidate = FindWindowExW(None, Some(pm), w!("WorkerW"), PCWSTR::null())
                        .ok()
                        .and_then(non_invalid);
                    return candidate.filter(|hwnd| covers_monitor(*hwnd, monitor));
                }
            }
        }
        None
    }
}

/// 候选层清单,便于日志里一眼看清当前系统长什么样。
#[cfg(feature = "dev-tools")]
pub fn candidate_report() -> String {
    let mut out = String::new();
    let pm = progman();
    out.push_str(&format!("\n  Progman            : {}\n", diag::describe(pm)));
    match defview() {
        Some(dv) => out.push_str(&format!(
            "  SHELLDLL_DefView   : {}  (root={:?})\n",
            diag::describe(dv),
            diag::root_of(dv).0 as isize
        )),
        None => out.push_str("  SHELLDLL_DefView   : (无)\n"),
    }
    let workers = top_worker_windows();
    if workers.is_empty() {
        out.push_str("  顶层 WorkerW       : (无)\n");
    }
    for (i, w) in workers.iter().enumerate() {
        out.push_str(&format!("  顶层 WorkerW[{i}]     : {}\n", diag::describe(*w)));
    }
    match progman_child_worker() {
        Some(w) => out.push_str(&format!("  Progman 子 WorkerW : {}\n", diag::describe(w))),
        None => out.push_str("  Progman 子 WorkerW : (无)\n"),
    }
    match classic_background_worker() {
        Some(w) => out.push_str(&format!("  经典背景层         : {}\n", diag::describe(w))),
        None => out.push_str("  经典背景层         : (无)\n"),
    }
    out
}

/// 按策略解析出要挂载的桌面层窗口与 Z 序落点。
fn resolve_host(mode: AttachMode) -> Result<Host> {
    spawn_workerw();

    let progman = non_invalid(progman())
        .ok_or_else(|| anyhow!("找不到 Progman,当前不是标准 Windows 桌面会话"))?;
    let dv = defview();
    let classic = classic_background_worker();
    let workers = top_worker_windows();
    let pg_worker = progman_background_worker();

    let host = match mode {
        AttachMode::Progman => Host {
            hwnd: progman,
            z: ZPlace::Top,
            desc: "Progman(Z 序不变)".into(),
        },
        AttachMode::ProgmanBottom => Host {
            hwnd: progman,
            z: ZPlace::Bottom,
            desc: "Progman(沉到最底)".into(),
        },
        AttachMode::BelowDefView => Host {
            hwnd: progman,
            z: dv.map_or(ZPlace::Bottom, ZPlace::Below),
            desc: format!("Progman(压在 DefView {:?} 之下)", dv.map(|d| d.0 as isize)),
        },
        AttachMode::ProgmanWorker => Host {
            hwnd: pg_worker.unwrap_or(progman),
            z: ZPlace::Top,
            desc: format!("Progman 背景层 WorkerW(命中={})", pg_worker.is_some()),
        },
        AttachMode::Workerw => Host {
            hwnd: classic.or(pg_worker).unwrap_or(progman),
            z: ZPlace::Top,
            desc: format!(
                "顶层 WorkerW(classic={}, Progman背景层={}, 顶层候选={})",
                classic.is_some(),
                pg_worker.is_some(),
                workers.len()
            ),
        },
        AttachMode::Auto => {
            // 最理想:经典背景层(和图标层是兄弟关系,天然不挡图标)
            if let Some(w) = classic {
                Host {
                    hwnd: w,
                    z: ZPlace::Top,
                    desc: "auto → 经典背景层 WorkerW".into(),
                }
            } else if let Some(w) = pg_worker {
                Host {
                    hwnd: w,
                    z: ZPlace::Top,
                    desc: "auto → Progman 背景层 WorkerW".into(),
                }
            } else {
                Host {
                    hwnd: progman,
                    z: dv.map_or(ZPlace::Bottom, ZPlace::Below),
                    desc: "auto → Progman(压 DefView 之下)".into(),
                }
            }
        }
    };

    Ok(host)
}

/// 窗口样式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostStyle {
    /// 壁纸宿主:无边框、不抢焦点、不进任务栏
    Wallpaper,
    /// 普通可激活窗口(排查用,用来对比 DXGI 呈现行为)
    Plain,
}

/// 壁纸宿主窗口:一个无边框、不抢焦点、不出现在任务栏的顶层窗口。
pub struct HostWindow {
    hwnd: HWND,
}

impl HostWindow {
    pub fn create(rect: Rect) -> Result<Self> {
        Self::create_with(rect, HostStyle::Wallpaper)
    }

    pub fn create_with(rect: Rect, style: HostStyle) -> Result<Self> {
        unsafe {
            let module = GetModuleHandleW(None)?;
            let instance = HINSTANCE(module.0);
            let brush = CreateSolidBrush(COLORREF(0x0014_1010));

            let class = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(wnd_proc),
                hInstance: instance,
                hbrBackground: brush,
                lpszClassName: HOST_CLASS,
                ..Default::default()
            };
            // 重复注册返回 0,属于正常情况
            RegisterClassExW(&class);

            let (x, y, width, height) = rect;
            let (ex_style, window_style) = match style {
                HostStyle::Wallpaper => (WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW, WS_POPUP),
                HostStyle::Plain => (WINDOW_EX_STYLE::default(), WS_OVERLAPPEDWINDOW),
            };
            let hwnd = CreateWindowExW(
                ex_style,
                HOST_CLASS,
                w!("MCWP"),
                window_style,
                x,
                y,
                width,
                height,
                None,
                None,
                Some(instance),
                None,
            )?;
            Ok(Self { hwnd })
        }
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// 挂到桌面壁纸层并铺满主显示器,返回实际使用的目标说明。
    ///
    /// `show_host` 会额外对目标层调用 `ShowWindow`(Win11 上顶层 WorkerW 常是隐藏的)。
    pub fn attach_to_desktop(&self, mode: AttachMode, show_host: bool) -> Result<String> {
        let host = resolve_host(mode)?;
        let (x, y, width, height) = primary_monitor_rect();
        // 子窗口的 `SetWindowPos` 用的是父窗口客户区坐标,而不是屏幕坐标。
        // Progman 覆盖整个虚拟屏幕(多屏时左上角是负坐标),换算错了窗口就会跑到屏幕外。
        let mut origin = POINT { x, y };
        unsafe {
            SetParent(self.hwnd, Some(host.hwnd))?;
            if !ScreenToClient(host.hwnd, &mut origin).as_bool() {
                anyhow::bail!("ScreenToClient 失败,无法把屏幕坐标换算成目标层客户区坐标");
            }
            let insert_after = match host.z {
                ZPlace::Top => Some(HWND_TOP),
                ZPlace::Bottom => Some(HWND_BOTTOM),
                ZPlace::Below(h) => Some(h),
            };
            SetWindowPos(
                self.hwnd,
                insert_after,
                origin.x,
                origin.y,
                width,
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )?;
            // 别对 Progman 调 ShowWindow,那会把整个桌面藏起来
            if show_host && host.hwnd != progman() {
                let _ = ShowWindow(host.hwnd, SW_SHOWNOACTIVATE);
            }
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
        tracing::info!(
            "壁纸窗口挂载完成:模式={}, 目标={}, 客户区坐标=({}, {}), 尺寸={width}x{height}",
            mode.as_str(),
            host.desc,
            origin.x,
            origin.y
        );
        Ok(host.desc)
    }

    /// 不进壁纸层,直接当普通窗口显示(调试用)。
    pub fn show_standalone(&self) -> Result<()> {
        let (x, y, width, height) = primary_monitor_rect();
        unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )?;
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
        Ok(())
    }

    /// 非阻塞地消费消息队列;收到 `WM_QUIT` 返回 `false`。
    pub fn pump_pending(&self) -> bool {
        let mut msg = MSG::default();
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return false;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        true
    }

    /// 阻塞式消息循环,直到 `deadline` 到期或收到 `WM_QUIT`(自检用)。
    pub fn run_until(&self, deadline: Instant) -> bool {
        while Instant::now() < deadline {
            if !self.pump_pending() {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(8));
        }
        true
    }
}

impl Drop for HostWindow {
    fn drop(&mut self) {
        if !self.hwnd.0.is_null() {
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_ERASEBKGND if PAINT_BACKGROUND.load(Ordering::Relaxed) => {
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            if PAINT_BACKGROUND.load(Ordering::Relaxed) {
                return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
            }
            let _ = unsafe { ValidateRect(Some(hwnd), None) };
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}
