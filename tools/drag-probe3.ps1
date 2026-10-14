Add-Type -Namespace T4 -Name M4 -MemberDefinition @"
[DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
[DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, System.IntPtr e);
[DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
[DllImport("user32.dll")] public static extern bool GetWindowRect(System.IntPtr h, out RECT r);
[DllImport("user32.dll")] public static extern bool IsZoomed(System.IntPtr h);
[DllImport("user32.dll")] public static extern bool IsIconic(System.IntPtr h);
[DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(System.IntPtr h, System.Text.StringBuilder t, int n);
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, System.IntPtr p);
public delegate bool EnumProc(System.IntPtr h, System.IntPtr p);
public struct POINT { public int X, Y; }
public struct RECT { public int L, T, R, B; }
"@
$found = [IntPtr]::Zero
$cb = [T4.M4+EnumProc]{ param($h,$p)
  $sb = New-Object System.Text.StringBuilder 256
  [void][T4.M4]::GetWindowTextW($h,$sb,256)
  if ($sb.ToString() -like "*控制面板*") { $script:found = $h; return $false }
  return $true }
[void][T4.M4]::EnumWindows($cb, [IntPtr]::Zero)
if ($found -eq [IntPtr]::Zero) { Write-Error 'no panel'; exit 1 }
function Rect { $r = New-Object T4.M4+RECT; [void][T4.M4]::GetWindowRect($found, [ref]$r); $r }
$cur = New-Object T4.M4+POINT; [void][T4.M4]::GetCursorPos([ref]$cur)

# —— 用例 1:快速甩动(每步 40px / 5ms) ——
$b = Rect
$gx = [int](($b.L + $b.R) / 2); $gy = [int]($b.T + 33)
[T4.M4]::SetCursorPos($gx, $gy) | Out-Null; Start-Sleep -Milliseconds 250
[T4.M4]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero); Start-Sleep -Milliseconds 120
for ($i=1; $i -le 8; $i++) { [T4.M4]::SetCursorPos($gx + $i*40, $gy) | Out-Null; Start-Sleep -Milliseconds 5 }
[T4.M4]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero); Start-Sleep -Milliseconds 350
$a = Rect
"用例1 快速甩动: 期望=(320,0) 实际=({0},{1})" -f ($a.L-$b.L), ($a.T-$b.T)

# —— 用例 2:甩到屏幕上边缘(Win11 贴边最大化手势) ——
$b = Rect
$gx = [int](($b.L + $b.R) / 2); $gy = [int]($b.T + 33)
[T4.M4]::SetCursorPos($gx, $gy) | Out-Null; Start-Sleep -Milliseconds 250
[T4.M4]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero); Start-Sleep -Milliseconds 120
for ($y = $gy; $y -ge 0; $y -= 20) { [T4.M4]::SetCursorPos($gx, $y) | Out-Null; Start-Sleep -Milliseconds 15 }
[T4.M4]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero); Start-Sleep -Milliseconds 400
$a = Rect
"用例2 拖到顶边: 最大化={0} 最小化={1} 位置=({2},{3}) 尺寸=({4}x{5}) (面板本体 940x648)" -f ([T4.M4]::IsZoomed($found)), ([T4.M4]::IsIconic($found)), $a.L, $a.T, ($a.R-$a.L), ($a.B-$a.T)
[T4.M4]::SetCursorPos($cur.X, $cur.Y) | Out-Null