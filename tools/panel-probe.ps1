# 面板回归探针:把面板临时提到最前,按脚本点击 / 截图,结束后恢复 z 序。
#
# 为什么需要它:单纯 SetCursorPos + mouse_event 会点到压在面板上面的窗口上,
# 页面状态就会"莫名其妙"没变。这里先把面板排到 z 序最前。
#
# **用 `HWND_TOP`,不用 `HWND_TOPMOST`**:后者留下的是持久样式 `WS_EX_TOPMOST`,脚本退出
# 也不消失(2026-10-16 的"面板总在最上层"就是这么来的,详见 raise-window.ps1 与方案 §11.19)。
#
# 开头那次 `HWND_NOTOPMOST` 是清历史遗留 —— 老版本脚本钉上去的状态,由这里顺手解开。
#
# 用法:
#   tools\panel-probe.ps1 -Actions 'click:470,78','shot:tab-1.png','click:171,78','shot:tab-back.png'
# 坐标是窗口内相对坐标;shot 路径相对 app\shots\。
param(
    [Parameter(Mandatory=$true)][string[]]$Actions,
    [string]$MatchTitle = '控制面板',
    [string]$ShotDir = 'G:\MC壁纸\app\shots',
    [int]$Pad = 12
)

Add-Type -Namespace Pp -Name W -MemberDefinition @"
[DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
[DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, System.IntPtr e);
[DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
[DllImport("user32.dll")] public static extern bool GetWindowRect(System.IntPtr h, out RECT r);
[DllImport("user32.dll")] public static extern bool IsWindow(System.IntPtr h);
[DllImport("user32.dll")] public static extern bool SetWindowPos(System.IntPtr h, System.IntPtr after, int x, int y, int cx, int cy, uint flags);
[DllImport("user32.dll")] public static extern bool ShowWindow(System.IntPtr h, int cmd);
[DllImport("user32.dll")] public static extern bool SetForegroundWindow(System.IntPtr h);
[DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(System.IntPtr v);
[DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(System.IntPtr h, System.Text.StringBuilder t, int n);
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, System.IntPtr p);
public delegate bool EnumProc(System.IntPtr h, System.IntPtr p);
public struct POINT { public int X, Y; }
public struct RECT { public int L, T, R, B; }
"@

[Pp.W]::SetProcessDpiAwarenessContext([IntPtr]::new(-4)) | Out-Null

$script:found = [IntPtr]::Zero
$cb = [Pp.W+EnumProc]{ param($h,$p)
    $sb = New-Object System.Text.StringBuilder 256
    [void][Pp.W]::GetWindowTextW($h,$sb,256)
    if ($sb.ToString() -like "*$MatchTitle*") { $script:found = $h; return $false }
    return $true }
[void][Pp.W]::EnumWindows($cb, [IntPtr]::Zero)
$h = $script:found
if ($h -eq [IntPtr]::Zero) { Write-Error "no panel window matching '$MatchTitle'"; exit 1 }

$HWND_TOP = [IntPtr]::Zero
$HWND_NOTOPMOST = [IntPtr]::new(-2)
$SWP = 0x0001 -bor 0x0002 -bor 0x0040   # NOSIZE | NOMOVE | SHOWWINDOW

[void][Pp.W]::ShowWindow($h, 5)
[void][Pp.W]::SetWindowPos($h, $HWND_NOTOPMOST, 0, 0, 0, 0, $SWP)
[void][Pp.W]::SetWindowPos($h, $HWND_TOP, 0, 0, 0, 0, $SWP)
[void][Pp.W]::SetForegroundWindow($h)
Start-Sleep -Milliseconds 250

$orig = New-Object Pp.W+POINT; [void][Pp.W]::GetCursorPos([ref]$orig)
Add-Type -AssemblyName System.Drawing

# 累加窗口移动量:窗口一旦被拖动,后面所有相对坐标都要跟着挪。
$shiftX = 0; $shiftY = 0

foreach ($a in $Actions) {
    if ($a -like 'click:*') {
        $xy = ($a.Substring(6) -split ',')
        $r = New-Object Pp.W+RECT; [void][Pp.W]::GetWindowRect($h, [ref]$r)
        $cx = $r.L + [int]$xy[0]; $cy = $r.T + [int]$xy[1]
        [void][Pp.W]::SetCursorPos($cx, $cy)
        Start-Sleep -Milliseconds 280
        [Pp.W]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)
        Start-Sleep -Milliseconds 60
        [Pp.W]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)
        Start-Sleep -Milliseconds 600
        "click ($($xy[0]),$($xy[1])) -> screen ($cx,$cy)"
    }
    elseif ($a -like 'drag:*') {
        # drag:x1,y1,x2,y2,steps —— 按住左键从窗口内 (x1,y1) 拖到 (x2,y2)
        $n = $a.Substring(5) -split ','
        $x1 = [int]$n[0]; $y1 = [int]$n[1]; $x2 = [int]$n[2]; $y2 = [int]$n[3]
        $steps = if ($n.Count -gt 4) { [int]$n[4] } else { 24 }
        $r = New-Object Pp.W+RECT; [void][Pp.W]::GetWindowRect($h, [ref]$r)
        $sx = $r.L + $x1; $sy = $r.T + $y1
        [void][Pp.W]::SetCursorPos($sx, $sy)
        Start-Sleep -Milliseconds 200
        [Pp.W]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)
        Start-Sleep -Milliseconds 80
        for ($i = 1; $i -le $steps; $i++) {
            $px = [int]($sx + ($x2 - $x1) * $i / $steps)
            $py = [int]($sy + ($y2 - $y1) * $i / $steps)
            [void][Pp.W]::SetCursorPos($px, $py)
            Start-Sleep -Milliseconds 12
        }
        Start-Sleep -Milliseconds 120
        [Pp.W]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)
        Start-Sleep -Milliseconds 500
        $shiftX += ($x2 - $x1); $shiftY += ($y2 - $y1)
        "drag ($x1,$y1)->($x2,$y2) window shift now ($shiftX,$shiftY)"
    }
    elseif ($a -like 'shot:*') {
        $name = $a.Substring(5)
        $out = if ([System.IO.Path]::IsPathRooted($name)) { $name } else { Join-Path $ShotDir $name }
        $r = New-Object Pp.W+RECT; [void][Pp.W]::GetWindowRect($h, [ref]$r)
        $x = $r.L - $Pad; $y = $r.T - $Pad
        $w = ($r.R - $r.L) + 2 * $Pad; $ht = ($r.B - $r.T) + 2 * $Pad
        $bmp = New-Object System.Drawing.Bitmap($w, $ht)
        $g = [System.Drawing.Graphics]::FromImage($bmp)
        $g.CopyFromScreen($x, $y, 0, 0, (New-Object System.Drawing.Size($w, $ht)))
        $g.Dispose()
        $dir = Split-Path -Parent $out
        if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir | Out-Null }
        $bmp.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
        $bmp.Dispose()
        "shot $out  rect=($($r.L),$($r.T))"
    }
    elseif ($a -like 'wait:*') {
        $ms = [int]$a.Substring(5); Start-Sleep -Milliseconds $ms; "wait $ms ms"
    }
    else { Write-Warning "unknown action: $a" }
}

# z 序不用"还原":`HWND_TOP` 只改了这一次的前后顺序,没给窗口留下任何样式。
[void][Pp.W]::SetCursorPos($orig.X, $orig.Y)
