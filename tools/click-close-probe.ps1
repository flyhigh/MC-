Add-Type -Namespace T5 -Name M5 -MemberDefinition @"
[DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
[DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, System.IntPtr e);
[DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
[DllImport("user32.dll")] public static extern bool GetWindowRect(System.IntPtr h, out RECT r);
[DllImport("user32.dll")] public static extern bool IsWindow(System.IntPtr h);
[DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(System.IntPtr h, System.Text.StringBuilder t, int n);
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, System.IntPtr p);
public delegate bool EnumProc(System.IntPtr h, System.IntPtr p);
public struct POINT { public int X, Y; }
public struct RECT { public int L, T, R, B; }
"@
function FindPanel { $script:found = [IntPtr]::Zero
  $cb = [T5.M5+EnumProc]{ param($h,$p)
    $sb = New-Object System.Text.StringBuilder 256
    [void][T5.M5]::GetWindowTextW($h,$sb,256)
    if ($sb.ToString() -like "*控制面板*") { $script:found = $h; return $false }
    return $true }
  [void][T5.M5]::EnumWindows($cb, [IntPtr]::Zero); return $script:found }
$h = FindPanel
if ($h -eq [IntPtr]::Zero) { Write-Error 'no panel'; exit 1 }
$r = New-Object T5.M5+RECT; [void][T5.M5]::GetWindowRect($h, [ref]$r)
$cur = New-Object T5.M5+POINT; [void][T5.M5]::GetCursorPos([ref]$cur)
# 关闭圆钮:标题栏右端 26x26(window 内坐标 x 896..922,y 20..46)→ 取中心
$cx = $r.L + 909; $cy = $r.T + 33
[T5.M5]::SetCursorPos($cx, $cy) | Out-Null
Start-Sleep -Milliseconds 400
[T5.M5]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero); Start-Sleep -Milliseconds 60
[T5.M5]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 1200
$r2 = New-Object T5.M5+RECT
$alive = [T5.M5]::IsWindow($h)
$null = $r2
[T5.M5]::SetCursorPos($cur.X, $cur.Y) | Out-Null
"点击关闭钮 @($cx,$cy): 窗口还在={0}" -f $alive