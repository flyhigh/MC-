# 把指定进程的主窗口提到最前(用于截图验证被遮挡的窗口)。
param(
    [Parameter(Mandatory=$true)][string]$ProcessName,
    [string]$MatchTitle
)

Add-Type -Namespace Mcwp -Name Win -MemberDefinition @"
[DllImport("user32.dll", SetLastError=true)] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
[DllImport("user32.dll", SetLastError=true)] public static extern bool SetForegroundWindow(IntPtr hWnd);
[DllImport("user32.dll", SetLastError=true)] public static extern bool ShowWindow(IntPtr hWnd, int cmd);
"@

$HWND_TOPMOST = [IntPtr]::new(-1)
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
[Mcwp.Win]::SetWindowPos($h, $HWND_TOPMOST, 0, 0, 0, 0, ($SWP_NOMOVE -bor $SWP_NOSIZE -bor $SWP_SHOWWINDOW)) | Out-Null
[Mcwp.Win]::SetForegroundWindow($h) | Out-Null
"raised: pid=$($target.Id) title='$($target.MainWindowTitle)'"
