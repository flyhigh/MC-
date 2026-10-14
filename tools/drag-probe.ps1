Add-Type -Namespace T -Name M -MemberDefinition @"
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
$found = [IntPtr]::Zero
$cb = [T.M+EnumProc]{ param($h,$p)
  $sb = New-Object System.Text.StringBuilder 256
  [void][T.M]::GetWindowTextW($h,$sb,256)
  if ($sb.ToString() -like "*控制面板*") { $script:found = $h; return $false }
  return $true }
[void][T.M]::EnumWindows($cb, [IntPtr]::Zero)
if ($found -eq [IntPtr]::Zero) { Write-Error 'no panel'; exit 1 }
$r0 = New-Object T.M+RECT; [void][T.M]::GetWindowRect($found, [ref]$r0)
$cur = New-Object T.M+POINT; [void][T.M]::GetCursorPos([ref]$cur)
$gx = [int](($r0.L + $r0.R) / 2); $gy = [int]($r0.T + 33)
[T.M]::SetCursorPos($gx, $gy) | Out-Null
Start-Sleep -Milliseconds 300
[T.M]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 200
# 40 步 x 6px,每步 20ms —— 模拟一次"慢速平稳拖拽"
for ($i=1; $i -le 40; $i++) { [T.M]::SetCursorPos($gx + $i*6, $gy + $i*2) | Out-Null; Start-Sleep -Milliseconds 20 }
Start-Sleep -Milliseconds 200
[T.M]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 400
$r1 = New-Object T.M+RECT; [void][T.M]::GetWindowRect($found, [ref]$r1)
[T.M]::SetCursorPos($cur.X, $cur.Y) | Out-Null
"期望位移=(240,80)  实际位移=({0},{1})  窗口最终=({2},{3})" -f ($r1.L-$r0.L), ($r1.T-$r0.T), $r1.L, $r1.T