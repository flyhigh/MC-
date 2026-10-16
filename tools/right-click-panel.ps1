# 在控制面板里右键一下(窗口内相对坐标),用来验证右键菜单。
# 与 click-panel.ps1 同一套坐标规矩:进程先设成 DPI 感知,坐标才是"截图上量到的那个"。
param(
    [Parameter(Mandatory=$true)][int]$X,
    [Parameter(Mandatory=$true)][int]$Y
)

Add-Type -Namespace RC -Name M -MemberDefinition @"
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

[RC.M]::SetProcessDpiAwarenessContext([IntPtr]::new(-4)) | Out-Null

$script:found = [IntPtr]::Zero
$cb = [RC.M+EnumProc]{ param($h,$p)
    $sb = New-Object System.Text.StringBuilder 256
    [void][RC.M]::GetWindowTextW($h,$sb,256)
    if ($sb.ToString() -like "*控制面板*") { $script:found = $h; return $false }
    return $true }
[void][RC.M]::EnumWindows($cb, [IntPtr]::Zero)
$h = $script:found
if ($h -eq [IntPtr]::Zero) { Write-Error 'no panel'; exit 1 }

$r = New-Object RC.M+RECT; [void][RC.M]::GetWindowRect($h, [ref]$r)
$cur = New-Object RC.M+POINT; [void][RC.M]::GetCursorPos([ref]$cur)

$cx = $r.L + $X; $cy = $r.T + $Y
[RC.M]::SetCursorPos($cx, $cy) | Out-Null
Start-Sleep -Milliseconds 300
[RC.M]::mouse_event(0x0008, 0, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 60
[RC.M]::mouse_event(0x0010, 0, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 500
"right-clicked panel@($X,$Y) -> screen($cx,$cy)"