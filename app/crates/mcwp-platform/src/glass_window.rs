//! 无边框面板窗口的 Windows 收尾工作。
//!
//! Slint 的 `no-frame: true` 只做到"不要标题栏",剩下三件事得自己来:
//!
//! 1. **圆角**:软件渲染器走 softbuffer(32bpp DIB + BitBlt),窗口没有逐像素 alpha,
//!    所以圆角只能交给 DWM —— `DWMWA_WINDOW_CORNER_PREFERENCE`,自带抗锯齿。
//! 2. **暗色边框**:标题栏没了,但 DWM 还会给圆角窗口描一圈 1px 的浅色边,在深色玻璃上
//!    很突兀,用 `DWMWA_BORDER_COLOR = DWMWA_COLOR_NONE` 抹掉。
//! 3. **拖动**:没有标题栏就没有拖动区,这里用"按下时记住光标 + 窗口原点,之后按光标位移
//!    移动窗口"来实现。用 `GetCursorPos` 而不是窗口内坐标,是因为窗口跟着鼠标走之后,
//!    鼠标在窗口内的相对坐标会回到原位,增量为 0,拖不动。
//! 4. **最小化 / 放大**:系统那颗最小化键和标题栏一起没了,只能自己代劳(`minimize` /
//!    `WindowZoom`)。放大不能用 `SW_MAXIMIZE` —— 见 `WindowZoom::toggle` 的说明。

use anyhow::Result;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_ROUND, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetWindowRect, SW_MINIMIZE, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    SetWindowPos, ShowWindow,
};

/// DWM 的"不要边框"哨兵值(`DWMWA_COLOR_NONE`)。
const DWMWA_COLOR_NONE: u32 = 0xFFFF_FFFE;

/// 把 `HWND`(以 `isize` 传递,方便面板侧从 Slint 的窗口句柄里取出来)变成 windows-rs 类型。
fn hwnd_of(raw: isize) -> HWND {
    HWND(raw as *mut core::ffi::c_void)
}

/// 给窗口套上"液态玻璃"需要的 DWM 属性:圆角 + 暗色模式 + 取消系统描边。
///
/// 失败不致命(老系统上没有这些属性),返回 `Err` 让调用方记一条日志即可。
pub fn apply_glass(hwnd: isize) -> Result<()> {
    let hwnd = hwnd_of(hwnd);
    let corner = DWMWCP_ROUND;
    let dark = 1u32; // BOOL,TRUE

    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            core::ptr::from_ref(&corner).cast(),
            core::mem::size_of_val(&corner) as u32,
        )?;
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            core::ptr::from_ref(&dark).cast(),
            core::mem::size_of_val(&dark) as u32,
        )?;
        let none = DWMWA_COLOR_NONE;
        // 这个属性在 Win11 上才有;失败就当没有,不影响其它两项。
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            core::ptr::from_ref(&none).cast(),
            core::mem::size_of_val(&none) as u32,
        );
    }
    Ok(())
}

/// 一次手动窗口拖动:记住按下瞬间的光标位置与窗口原点,之后按光标位移搬窗口。
///
/// 这条是**兜底**路径:正常情况下面板把拖动交给 Windows 自己的标题栏模态移动循环
/// (`winit::Window::drag_window`),只有拿不到 winit 窗口时才用这里。
#[derive(Debug, Clone, Copy)]
pub struct WindowDrag {
    hwnd: isize,
    cursor: (i32, i32),
    origin: (i32, i32),
    /// 上一次真正写进系统的位置,用来丢掉"指针原地抖动"这种白搬。
    applied: (i32, i32),
}

impl WindowDrag {
    /// 开始拖动。拿不到光标/窗口位置时返回 `None`(就当这次拖不动)。
    pub fn begin(hwnd: isize) -> Option<Self> {
        let mut cursor = Default::default();
        let mut rect = RECT::default();
        unsafe {
            GetCursorPos(&mut cursor).ok()?;
            GetWindowRect(hwnd_of(hwnd), &mut rect).ok()?;
        }
        Some(Self {
            hwnd,
            cursor: (cursor.x, cursor.y),
            origin: (rect.left, rect.top),
            applied: (rect.left, rect.top),
        })
    }

    /// 按当前光标位置把窗口挪到该去的地方。
    pub fn update(&mut self) {
        let mut cursor = Default::default();
        if unsafe { GetCursorPos(&mut cursor) }.is_err() {
            return;
        }
        let next = (
            self.origin.0 + (cursor.x - self.cursor.0),
            self.origin.1 + (cursor.y - self.cursor.1),
        );
        // 每次搬窗口都要走一遍合成器与 DWM,位置没变就别白搬。
        if next == self.applied {
            return;
        }
        self.applied = next;
        unsafe {
            let _ = SetWindowPos(
                hwnd_of(self.hwnd),
                None,
                next.0,
                next.1,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
}

/// 最小化窗口(顶栏那颗「−」)。
///
/// 无边框窗口没有系统菜单,`ShowWindow` 是唯一入口;最小化后仍然能从任务栏恢复,因为
/// 面板窗口本来就有任务栏按钮(Slint 没把它做成 `WS_EX_TOOLWINDOW`)。
pub fn minimize(hwnd: isize) -> Result<()> {
    let _ = unsafe { ShowWindow(hwnd_of(hwnd), SW_MINIMIZE) };
    Ok(())
}

/// 面板的「放大 / 还原」状态。
///
/// 故意**不走 `SW_MAXIMIZE`**:窗口是 `no-frame`,没有 `WS_THICKFRAME`,系统算"最大化尺寸"
/// 的那套公式(工作区 + 边框)会退化成整块显示器 —— 最大化之后任务栏被盖住。所以这里自己
/// 量一次显示器工作区,摆过去;顺带把放大前那块矩形记下来当还原点。
#[derive(Debug, Default)]
pub struct WindowZoom {
    /// 放大前的窗口矩形;`None` = 当前没放大。
    restore: Option<RECT>,
}

impl WindowZoom {
    /// 在「放大」和「还原」之间切换,返回切换之后是不是放大状态。
    pub fn toggle(&mut self, hwnd: isize) -> Result<bool> {
        let hwnd = hwnd_of(hwnd);

        // 还原:摆回放大前那块矩形(宽高一起写,所以 `SWP_NOSIZE` 这次不能加)。
        if let Some(rect) = self.restore.take() {
            let _ = unsafe {
                SetWindowPos(
                    hwnd,
                    None,
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };
            return Ok(false);
        }

        let mut rect = RECT::default();
        unsafe { GetWindowRect(hwnd, &mut rect)? };

        let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
        let mut info = MONITORINFO {
            cbSize: core::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            return Ok(false);
        }

        let work = info.rcWork;
        self.restore = Some(rect);
        let _ = unsafe {
            SetWindowPos(
                hwnd,
                None,
                work.left,
                work.top,
                work.right - work.left,
                work.bottom - work.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
        };
        Ok(true)
    }
}
