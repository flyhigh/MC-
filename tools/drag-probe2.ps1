Add-Type -Namespace T3 -Name M3 -MemberDefinition @"
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
$cb = [T3.M3+EnumProc]{ param($h,$p)
  $sb = New-Object System.Text.StringBuilder 256
  [void][T3.M3]::GetWindowTextW($h,$sb,256)
  if ($sb.ToString() -like "*控制面板*") { $script:found = $h; return $false }
  return $true }
[void][T3.M3]::EnumWindows($cb, [IntPtr]::Zero)
if ($found -eq [IntPtr]::Zero) { Write-Error 'no panel'; exit 1 }
function Rect { $r = New-Object T3.M3+RECT; [void][T3.M3]::GetWindowRect($found, [ref]$r); $r }
$cur = New-Object T3.M3+POINT; [void][T3.M3]::GetCursorPos([ref]$cur)
$r = Rect
$gx = [int](($r.L + $r.R) / 2); $gy = [int]($r.T + 33)
for ($d = 1; $d -le 3; $d++) {
  $before = Rect
  # 每次都按窗口当前位置重新算抓取点:窗口已经被上一次拖动挪走了
  $gx = [int](($before.L + $before.R) / 2); $gy = [int]($before.T + 33)
  [T3.M3]::SetCursorPos($gx, $gy) | Out-Null
  Start-Sleep -Milliseconds 250
  [T3.M3]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)
  Start-Sleep -Milliseconds 150
  $bx = $gx; $by = $gy
  for ($i=1; $i -le 25; $i++) { [T3.M3]::SetCursorPos($bx + $i*4, $by + $i*2) | Out-Null; Start-Sleep -Milliseconds 15 }
  Start-Sleep -Milliseconds 120
  [T3.M3]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)
  Start-Sleep -Milliseconds 350
  $after = Rect
  "第 $d 次拖动: 期望=(100,50) 实际=({0},{1}) 位置=({2},{3})" -f ($after.L-$before.L), ($after.T-$before.T), $after.L, $after.T
}
[T3.M3]::SetCursorPos($cur.X, $cur.Y) | Out-Null