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

use anyhow::Result;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    DwmSetWindowAttribute,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetWindowRect, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos,
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
