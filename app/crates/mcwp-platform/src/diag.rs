//! 桌面窗口层级诊断。
//!
//! 只在排查阶段使用(`--dump` / `--dev-tools`),正式运行路径不依赖它。
//! 作用:把「桌面这一层的窗口都是谁、多大、可见与否、谁是谁的子窗口」直接打出来。

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetAncestor, GetClassNameW, GetParent, GetWindow, GetWindowLongPtrW,
    GetWindowRect, GetWindowTextW, IsWindowVisible, GA_ROOT, GWL_EXSTYLE, GWL_STYLE, GW_CHILD,
    GW_HWNDNEXT, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_DISABLED, WS_POPUP, WS_VISIBLE,
    WS_EX_APPWINDOW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
};

pub fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    if n <= 0 {
        String::new()
    } else {
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

pub fn window_text(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    if n <= 0 {
        String::new()
    } else {
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

/// `(left, top, right, bottom)`,取不到时返回全 0。
pub fn window_rect(hwnd: HWND) -> (i32, i32, i32, i32) {
    let mut r = windows::Win32::Foundation::RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut r) }.is_err() {
        return (0, 0, 0, 0);
    }
    (r.left, r.top, r.right, r.bottom)
}

pub fn parent_of(hwnd: HWND) -> HWND {
    unsafe { GetParent(hwnd) }.unwrap_or_default()
}

pub fn root_of(hwnd: HWND) -> HWND {
    unsafe { GetAncestor(hwnd, GA_ROOT) }
}

pub fn visible(hwnd: HWND) -> bool {
    unsafe { IsWindowVisible(hwnd).as_bool() }
}

pub fn style_of(hwnd: HWND) -> u32 {
    unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 }
}

pub fn ex_style_of(hwnd: HWND) -> u32 {
    unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 }
}

fn flags(value: u32, table: &[(u32, &str)]) -> String {
    let mut out: Vec<&str> = Vec::new();
    for (bit, name) in table {
        if value & bit != 0 {
            out.push(name);
        }
    }
    if out.is_empty() {
        "-".to_string()
    } else {
        out.join("|")
    }
}

pub fn style_text(value: u32) -> String {
    flags(
        value,
        &[
            (WS_CHILD.0, "CHILD"),
            (WS_POPUP.0, "POPUP"),
            (WS_VISIBLE.0, "VISIBLE"),
            (WS_CLIPSIBLINGS.0, "CLIPSIBLINGS"),
            (WS_CLIPCHILDREN.0, "CLIPCHILDREN"),
            (WS_DISABLED.0, "DISABLED"),
        ],
    )
}

pub fn ex_style_text(value: u32) -> String {
    flags(
        value,
        &[
            (WS_EX_NOACTIVATE.0, "NOACTIVATE"),
            (WS_EX_TOOLWINDOW.0, "TOOLWINDOW"),
            (WS_EX_TOPMOST.0, "TOPMOST"),
            (WS_EX_LAYERED.0, "LAYERED"),
            (WS_EX_TRANSPARENT.0, "TRANSPARENT"),
            (WS_EX_APPWINDOW.0, "APPWINDOW"),
            (WS_EX_NOREDIRECTIONBITMAP.0, "NOREDIRECTIONBITMAP"),
        ],
    )
}

/// 一行摘要。
pub fn describe(hwnd: HWND) -> String {
    let (l, t, r, b) = window_rect(hwnd);
    let style = style_of(hwnd);
    let ex = ex_style_of(hwnd);
    let text = window_text(hwnd);
    let mut out = format!(
        "{:<22} hwnd={:>12} rect=({l:>6},{t:>6})-({r:>6},{b:>6}) {:>5}x{:<5} vis={:<5} style={:<40} ex={:<45} parent={:>12}",
        class_name(hwnd),
        hwnd.0 as isize,
        r - l,
        b - t,
        visible(hwnd).to_string(),
        style_text(style),
        ex_style_text(ex),
        parent_of(hwnd).0 as isize,
    );
    if !text.is_empty() {
        out.push_str(&format!(" title={text:?}"));
    }
    out
}

/// 从窗口自身一路打印到 root,用来确认 `SetParent` 到底成功了没有。
pub fn describe_chain(hwnd: HWND, label: &str) {
    println!("-- {label}");
    println!("   self      : {}", describe(hwnd));
    let parent = parent_of(hwnd);
    if parent.0.is_null() {
        println!("   parent    : (无,仍是顶层窗口)");
    } else {
        println!("   parent    : {}", describe(parent));
        let grand = parent_of(parent);
        if !grand.0.is_null() {
            println!("   grandparent: {}", describe(grand));
        }
    }
    let root = root_of(hwnd);
    if root.0 != hwnd.0 {
        println!("   root      : {}", describe(root));
    }
}

/// 打印全部顶层窗口及其子孙(最多 `max_depth` 层)。
pub fn dump_tree(max_depth: usize) {
    println!("== 桌面窗口层级(缩进=子窗口)==");
    unsafe {
        let _ = EnumWindows(Some(enum_top), LPARAM(max_depth as isize));
    }
}

unsafe extern "system" fn enum_top(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let depth = lparam.0 as usize;
    unsafe { dump_one(hwnd, 0, depth) };
    BOOL(1)
}

unsafe fn dump_one(hwnd: HWND, depth: usize, max_depth: usize) {
    println!("{}{}", "  ".repeat(depth), describe(hwnd));
    if depth >= max_depth {
        return;
    }
    let mut child = unsafe { GetWindow(hwnd, GW_CHILD) }.unwrap_or_default();
    while !child.0.is_null() {
        unsafe { dump_one(child, depth + 1, max_depth) };
        let next = unsafe { GetWindow(child, GW_HWNDNEXT) }.unwrap_or_default();
        if next.0 == child.0 {
            break;
        }
        child = next;
    }
}
