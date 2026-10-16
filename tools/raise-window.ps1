# 把指定进程的主窗口提到最前(用于截图 / 点击验证被遮挡的窗口)。
#
# **别用 `HWND_TOPMOST`。** 它给窗口加上的是 `WS_EX_TOPMOST` —— 一个**持久样式**,脚本早就
# 退出了它还在,窗口会一直压在别的应用上面,直到进程结束。2026-10-16 就是这么翻的车:为了
# 截图跑了几次这个脚本,结果用户看到的是"面板为什么总在最上层,遮盖其他应用"。
# `HWND_TOP` 才是这里想要的语义:"这一次排到 z 序最前",用户点别的窗口照样能盖住它。
#
# 开头那次 `HWND_NOTOPMOST` 是**清历史遗留**:老版本脚本钉上去的状态,由这个工具顺手解开。
param(
    [Parameter(Mandatory=$true)][string]$ProcessName,
    [string]$MatchTitle
)

Add-Type -Namespace Mcwp -Name Win -MemberDefinition @"
[DllImport("user32.dll", SetLastError=true)] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
[DllImport("user32.dll", SetLastError=true)] public static extern bool SetForegroundWindow(IntPtr hWnd);
[DllImport("user32.dll", SetLastError=true)] public static extern bool ShowWindow(IntPtr hWnd, int cmd);
[DllImport("user32.dll", SetLastError=true)] public static extern IntPtr GetWindowLongPtrW(IntPtr hWnd, int idx);
"@

$HWND_TOP = [IntPtr]::Zero
$HWND_NOTOPMOST = [IntPtr]::new(-2)
$SWP_NOMOVE = 0x0002; $SWP_NOSIZE = 0x0001; $SWP_SHOWWINDOW = 0x0040

$target = $null
foreach ($p in Get-Process -Name $ProcessName -ErrorAction SilentlyContinue) {
    if ($p.MainWindowHandle -ne 0) {
        if (-not $MatchTitle) { $target = $p; break }
        if ($p.MainWindowTitle -like "*$MatchTitle*") { $target = $p; break }
    }
}
if (-not $target) { "没找到窗口: $ProcessName"; exit 1 }

# 9 = SW_RESTORE:窗口被最小化过时,SW_SHOW(5) 只是让它"保持最小化",得用 RESTORE 才拉得回来。
$h = $target.MainWindowHandle
[Mcwp.Win]::ShowWindow($h, 9) | Out-Null

# 钉住的状态**要说出来**:悄悄解开的话,下次再有人写出 `HWND_TOPMOST`,现象还是"面板总在最上层",
# 但没人知道是谁钉的。这一行是给下一个人(也可能是三个月后的自己)留的线索。
$WS_EX_TOPMOST = 0x00000008
if (([Mcwp.Win]::GetWindowLongPtrW($h, -20).ToInt64() -band $WS_EX_TOPMOST) -ne 0) {
    "注意:这个窗口原来被钉成了 TOPMOST(WS_EX_TOPMOST),正在解开"
}
[Mcwp.Win]::SetWindowPos($h, $HWND_NOTOPMOST, 0, 0, 0, 0, ($SWP_NOMOVE -bor $SWP_NOSIZE -bor $SWP_SHOWWINDOW)) | Out-Null
[Mcwp.Win]::SetWindowPos($h, $HWND_TOP, 0, 0, 0, 0, ($SWP_NOMOVE -bor $SWP_NOSIZE -bor $SWP_SHOWWINDOW)) | Out-Null
[Mcwp.Win]::SetForegroundWindow($h) | Out-Null
"raised: pid=$($target.Id) title='$($target.MainWindowTitle)'"
