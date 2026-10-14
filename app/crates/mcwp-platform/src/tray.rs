//! 任务栏托盘图标 + 右键菜单。
//!
//! 这里不需要另起线程,也不需要自己的消息循环:宿主窗口的 `pump_pending()` 用的是
//! `PeekMessageW(None, ...)`,取的是**整条线程**的消息队列,所以托盘窗口只要跟宿主建在
//! 同一个线程上,它的 `WM_APP` 回调就会被那个循环顺带派发。托盘模块因此只做三件事:
//! 建一个隐藏窗口、注册图标、在右键时弹菜单,把用户选了什么塞进一个队列等主循环来取。
//!
//! 菜单项的行为一律不在这里执行:`TrayAction` 只是"用户点了什么",怎么落地由壁纸主循环
//! 决定 —— 跟面板的 `UiEvent` 是同一个套路。

use std::mem::size_of;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use windows::core::{PCWSTR, w};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_SETVERSION,
    NOTIFYICONDATAW, NOTIFYICONDATAW_0, NOTIFYICON_VERSION_4, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyIcon, DestroyMenu, DestroyWindow, GetCursorPos, HICON,
    LR_DEFAULTCOLOR, MENU_ITEM_FLAGS, MF_CHECKED, MF_SEPARATOR, MF_STRING, PostMessageW,
    RegisterClassW, SetForegroundWindow, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    TrackPopupMenu, WM_APP, WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL,
    WM_RBUTTONUP, WNDCLASSW, WINDOW_EX_STYLE, WS_POPUP,
};

/// 托盘窗口的类名。
const TRAY_CLASS: PCWSTR = w!("MCWP_TrayIcon");
/// 图标 id。一台机器上只挂一个,固定即可。
const ICON_ID: u32 = 1;
/// 托盘回调消息。`WM_APP` 起是给应用自己用的区间。
const TRAY_CALLBACK: u32 = WM_APP + 1;

const CMD_OPEN_PANEL: u32 = 1;
const CMD_NEXT_PRESET: u32 = 2;
const CMD_TOGGLE_PAUSE: u32 = 3;
const CMD_TOGGLE_AUTOSTART: u32 = 4;
const CMD_QUIT: u32 = 5;

/// 用户在托盘菜单里选了什么。执行落在壁纸主循环里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// 打开控制面板(没开就开一个,开着就叫到前台)
    OpenPanel,
    /// 切到下一个内置预设
    NextPreset,
    /// 暂停 / 继续渲染
    TogglePause,
    /// 开关开机自启
    ToggleAutostart,
    /// 退出整个进程
    Quit,
}

/// 窗口过程是自由函数,只能靠静态队列把动作递出去。
static PENDING: Mutex<Vec<TrayAction>> = Mutex::new(Vec::new());
/// 当前是否暂停渲染。菜单文案要跟着变,由主循环通过 [`set_paused`] 同步。
static PAUSED: AtomicBool = AtomicBool::new(false);
/// 图标是否按版本 4 的回调约定注册(右键走 `WM_CONTEXTMENU` 而不是 `WM_RBUTTONUP`)。
static VERSION4: AtomicBool = AtomicBool::new(false);

/// 主循环每帧调一次,取走这一轮攒下的托盘动作。
pub fn drain_actions() -> Vec<TrayAction> {
    PENDING
        .lock()
        .map(|mut queue| std::mem::take(&mut *queue))
        .unwrap_or_default()
}

/// 同步「暂停中」这个状态,只影响菜单文案。
pub fn set_paused(paused: bool) {
    PAUSED.store(paused, Ordering::Relaxed);
}

fn push(action: TrayAction) {
    if let Ok(mut queue) = PENDING.lock() {
        queue.push(action);
    }
}

/// 托盘图标。活到 `drop` 为止;`drop` 会把图标、窗口、HICON 一并收干净。
pub struct Tray {
    hwnd: HWND,
    hicon: HICON,
}

impl Tray {
    /// 注册托盘图标。必须在有消息循环的线程上调用(壁纸主线程即可)。
    ///
    /// `icon_ico` 是一份 .ico 文件的字节(Cargo 里 `include_bytes!` 进来的,不落盘)。
    pub fn install(icon_ico: &[u8], tip: &str) -> Result<Self> {
        let hicon = load_icon(icon_ico)
            .ok_or_else(|| anyhow::anyhow!("从 .ico 字节里没能取出可用的图标"))?;

        let hwnd = unsafe {
            let module = GetModuleHandleW(None)?;
            let instance = HINSTANCE(module.0);
            let class = WNDCLASSW {
                lpfnWndProc: Some(tray_wnd_proc),
                hInstance: instance,
                lpszClassName: TRAY_CLASS,
                ..Default::default()
            };
            // 重复注册返回 0,属于正常情况。
            RegisterClassW(&class);
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                TRAY_CLASS,
                w!("MCWP_Tray"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance),
                None,
            )
        };

        let hwnd = match hwnd {
            Ok(hwnd) => hwnd,
            Err(err) => {
                unsafe { let _ = DestroyIcon(hicon); }
                return Err(err).context("创建托盘窗口失败");
            }
        };

        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: ICON_ID,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
            uCallbackMessage: TRAY_CALLBACK,
            hIcon: hicon,
            ..Default::default()
        };
        fill_utf16(&mut data.szTip, tip);

        if !unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
            // NIM_ADD 失败(资源管理器没起来之类),把已经建的东西收掉再报错。
            unsafe {
                let _ = DestroyIcon(hicon);
                let _ = DestroyWindow(hwnd);
            }
            anyhow::bail!("Shell_NotifyIconW(NIM_ADD) 失败,托盘图标没挂上");
        }

        // 版本 4:右键派发 `WM_CONTEXTMENU`(低 16 位),并且从 Vista 起提示气泡的行为才正常。
        let mut versioned = data;
        versioned.Anonymous = NOTIFYICONDATAW_0 {
            uVersion: NOTIFYICON_VERSION_4,
        };
        let version4 = unsafe { Shell_NotifyIconW(NIM_SETVERSION, &versioned) }.as_bool();
        VERSION4.store(version4, Ordering::Relaxed);
        if !version4 {
            tracing::warn!("托盘图标没接受版本 4 回调,右键改走 WM_RBUTTONUP");
        }

        Ok(Self { hwnd, hicon })
    }

    /// 托盘窗口句柄(诊断用)。
    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            let data = NOTIFYICONDATAW {
                cbSize: size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: self.hwnd,
                uID: ICON_ID,
                ..Default::default()
            };
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
            let _ = DestroyIcon(self.hicon);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

unsafe extern "system" fn tray_wnd_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        TRAY_CALLBACK => {
            // 版本 4 的约定:lparam 低 16 位是事件消息,高 16 位是图标 id。
            let event = (lparam.0 as u32) & 0xFFFF;
            let context_event = if VERSION4.load(Ordering::Relaxed) {
                WM_CONTEXTMENU
            } else {
                WM_RBUTTONUP
            };
            if event == context_event {
                if let Some(action) = unsafe { popup_menu(hwnd) } {
                    push(action);
                }
            } else if event == WM_LBUTTONUP || event == WM_LBUTTONDBLCLK {
                push(TrayAction::OpenPanel);
            }
            LRESULT(0)
        }
        // 辅助窗口:不要 PostQuitMessage,退出与否由宿主窗口决定。
        WM_DESTROY => LRESULT(0),
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

/// 在光标处弹菜单,返回用户选的那一项。
///
/// 调用约定是微软文档里的固定套路:先把本窗口设为前台(否则菜单点到外面不会自动关),
/// 拿到返回值后补一条 `WM_NULL` 把前台还回去。
unsafe fn popup_menu(hwnd: HWND) -> Option<TrayAction> {
    let menu = unsafe { CreatePopupMenu() }.ok()?;
    let paused = PAUSED.load(Ordering::Relaxed);
    let autostart = crate::autostart::is_enabled();
    let checked = |on: bool| {
        if on {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        }
    };

    unsafe {
        let _ = AppendMenuW(menu, MF_STRING, CMD_OPEN_PANEL as usize, w!("打开控制面板"));
        let _ = AppendMenuW(menu, MF_STRING, CMD_NEXT_PRESET as usize, w!("换一个预设"));
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            CMD_TOGGLE_PAUSE as usize,
            if paused { w!("继续渲染") } else { w!("暂停渲染") },
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING | checked(autostart),
            CMD_TOGGLE_AUTOSTART as usize,
            w!("开机自启"),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, CMD_QUIT as usize, w!("退出 MC 壁纸"));
    }

    let mut point = POINT::default();
    if unsafe { GetCursorPos(&mut point) }.is_err() {
        let _ = unsafe { DestroyMenu(menu) };
        return None;
    }

    let chosen = unsafe {
        let _ = SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
            point.x,
            point.y,
            None,
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        chosen
    };

    // 带 TPM_RETURNCMD 时,返回值直接就是命令 id(0 表示用户没选,点了别处)。
    match chosen.0 as u32 {
        CMD_OPEN_PANEL => Some(TrayAction::OpenPanel),
        CMD_NEXT_PRESET => Some(TrayAction::NextPreset),
        CMD_TOGGLE_PAUSE => Some(TrayAction::TogglePause),
        CMD_TOGGLE_AUTOSTART => Some(TrayAction::ToggleAutostart),
        CMD_QUIT => Some(TrayAction::Quit),
        _ => None,
    }
}

/// 从 .ico 里挑一条最合适的位图,交给 Win32 建 `HICON`。
///
/// .ico 是"目录 + 若干张图"的容器:6 字节文件头(保留位 / 类型 / 张数)后面跟着每张图
/// 16 字节的目录项,目录项里的 `image_offset` / `bytes_in_res` 就指向那张图的数据。
/// `CreateIconFromResourceEx` 要的正是**单张图**的数据,不是整个文件 —— 直接喂整个文件
/// 或者用 `ExtractIconW`(它按序号取,等于白丢后面的码流)都拿不到想要的那张。
///
/// 本工程的图标每条都是 PNG 压缩的 32bpp(Vista 起支持),所以这里不做解码,原样透传。
fn load_icon(ico: &[u8]) -> Option<HICON> {
    let (offset, length) = pick_icon_entry(ico, 32)?;
    let bits = ico.get(offset..offset.checked_add(length)?)?;
    unsafe {
        CreateIconFromResourceEx(
            bits,
            true,
            0x0003_0000,
            32,
            32,
            LR_DEFAULTCOLOR,
        )
    }
    .ok()
}

/// 在 .ico 目录里挑边长最接近 `want` 的一张,返回 `(偏移, 长度)`。
///
/// 优先挑不小于 `want` 里最小的那张(缩小比放大清楚),没有就用现有最大的。
fn pick_icon_entry(ico: &[u8], want: u16) -> Option<(usize, usize)> {
    let count = u16::from_le_bytes([*ico.get(4)?, *ico.get(5)?]) as usize;
    let mut best: Option<(u16, usize, usize)> = None;

    for index in 0..count {
        let base = 6 + index * 16;
        let entry = ico.get(base..base + 16)?;
        // 目录里宽高是单字节,0 代表 256(放不进一字节)。
        let side = match entry[0] {
            0 => 256u16,
            raw => raw as u16,
        };
        let length = u32::from_le_bytes(entry[8..12].try_into().ok()?) as usize;
        let offset = u32::from_le_bytes(entry[12..16].try_into().ok()?) as usize;
        if length == 0 || offset.checked_add(length)? > ico.len() {
            continue;
        }
        let better = match best {
            None => true,
            Some((current, _, _)) => {
                // 目标之内的:取更大的;都比目标大或都比目标小的时候同样取更接近的。
                let score = |value: u16| {
                    if value >= want {
                        (0u8, value - want)
                    } else {
                        (1u8, want - value)
                    }
                };
                score(side) < score(current)
            }
        };
        if better {
            best = Some((side, offset, length));
        }
    }

    best.map(|(_, offset, length)| (offset, length))
}

/// 把字符串写进定长 UTF-16 缓冲:截断而不是溢出,末尾补 0。给 `szTip` 这类字段用。
fn fill_utf16(buffer: &mut [u16], text: &str) {
    if buffer.is_empty() {
        return;
    }
    let mut written = 0;
    for unit in text.encode_utf16() {
        if written + 1 >= buffer.len() {
            break;
        }
        buffer[written] = unit;
        written += 1;
    }
    buffer[written] = 0;
}
