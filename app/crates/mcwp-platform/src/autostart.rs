//! 开机自启:在 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 下维护一条字符串值。
//!
//! 写 HKCU 而不是 HKLM:不需要提权,关掉的时候也只是删掉自己这一条,不留全局残留。
//! 值名固定,所以重复启用就是覆盖,启用/禁用完全可逆 —— 不碰启动文件夹、不建计划任务,
//! 用户在任何"启动项"管理界面里都能看到并关掉它。

use std::os::windows::ffi::OsStrExt;

use anyhow::{Context, Result};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
};

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE_NAME: PCWSTR = w!("MCWallpaper");

/// 现在是否已登记自启。
pub fn is_enabled() -> bool {
    let mut key = HKEY::default();
    let opened = unsafe {
        RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, KEY_QUERY_VALUE, &mut key)
    };
    if opened != ERROR_SUCCESS {
        return false;
    }
    let mut size = 0u32;
    let queried = unsafe {
        RegQueryValueExW(key, VALUE_NAME, None, None, None, Some(&mut size))
    };
    let _ = unsafe { RegCloseKey(key) };
    queried == ERROR_SUCCESS && size > 0
}

/// 打开(必要时创建)自启项。
pub fn set_enabled(enabled: bool) -> Result<()> {
    let mut key = HKEY::default();
    let created = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
    };
    if created != ERROR_SUCCESS {
        anyhow::bail!("打开自启注册表项失败(错误码 {})", created.0);
    }

    let status = if enabled {
        let exe = std::env::current_exe().context("取当前 exe 路径失败")?;
        // 注册表里的命令行走的是"命令行解析",路径带空格必须带引号,否则会被拆成多段。
        let command = format!("\"{}\"", exe.display());
        let wide: Vec<u16> = std::ffi::OsStr::new(&command)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let bytes = unsafe {
            std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), wide.len() * 2)
        };
        unsafe { RegSetValueExW(key, VALUE_NAME, None, REG_SZ, Some(bytes)) }
    } else {
        let removed = unsafe { RegDeleteValueW(key, VALUE_NAME) };
        // 本来就没登记,等同于"已经关掉了"。
        if removed == ERROR_FILE_NOT_FOUND {
            ERROR_SUCCESS
        } else {
            removed
        }
    };
    let _ = unsafe { RegCloseKey(key) };

    if status != ERROR_SUCCESS {
        anyhow::bail!(
            "{}开机自启失败(错误码 {})",
            if enabled { "写入" } else { "清除" },
            status.0
        );
    }
    tracing::info!("已{}开机自启", if enabled { "开启" } else { "关闭" });
    Ok(())
}
