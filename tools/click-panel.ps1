# 在控制面板里点一下(窗口内相对坐标),用来回归页签这类纯 UI 交互。
param(
    [Parameter(Mandatory=$true)][int]$X,
    [Parameter(Mandatory=$true)][int]$Y,
    [int]$HoldMs = 0
)

Add-Type -Namespace Tab -Name M -MemberDefinition @"
[DllImport("user32.dll", SetLastError=true)] public static extern bool SetProcessDpiAwarenessContext(System.IntPtr value);
[DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
[DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, System.IntPtr e);
[DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
[DllImport("user32.dll")] public static extern bool GetWindowRect(System.IntPtr h, out RECT r);
[DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(System.IntPtr h, System.Text.StringBuilder t, int n);
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, System.IntPtr p);
public delegate bool EnumProc(System.IntPtr h, System.IntPtr p);
public struct POINT { public int X, Y; }
public struct RECT { public int L, T, R, B; }
"@
 
# -4 = DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2。**必须**设置:否则 150% 缩放下
# GetWindowRect 拿到的是虚拟化后的坐标、SetCursorPos 也会被再换算一次,点哪儿都不对
# (截图工具已经是 DPI 感知的,两个工具的坐标系必须一致才能对着截图点)。
[Tab.M]::SetProcessDpiAwarenessContext([IntPtr]::new(-4)) | Out-Null

$script:found = [IntPtr]::Zero
$cb = [Tab.M+EnumProc]{ param($h,$p)
    $sb = New-Object System.Text.StringBuilder 256
    [void][Tab.M]::GetWindowTextW($h,$sb,256)
    if ($sb.ToString() -like "*控制面板*") { $script:found = $h; return $false }
    return $true }
[void][Tab.M]::EnumWindows($cb, [IntPtr]::Zero)
$h = $script:found
if ($h -eq [IntPtr]::Zero) { Write-Error 'no panel'; exit 1 }

$r = New-Object Tab.M+RECT; [void][Tab.M]::GetWindowRect($h, [ref]$r)
$cur = New-Object Tab.M+POINT; [void][Tab.M]::GetCursorPos([ref]$cur)

$cx = $r.L + $X; $cy = $r.T + $Y
[Tab.M]::SetCursorPos($cx, $cy) | Out-Null
Start-Sleep -Milliseconds 300
[Tab.M]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 60
[Tab.M]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)
if ($HoldMs -gt 0) { Start-Sleep -Milliseconds $HoldMs }
Start-Sleep -Milliseconds 500
[Tab.M]::SetCursorPos($cur.X, $cur.Y) | Out-Null
"clicked panel@($X,$Y) -> screen($cx,$cy)"
