# 只截控制面板窗口(带留白),用于快速对照 UI 改动。
param(
    [Parameter(Mandatory=$true)][string]$Out,
    [string]$MatchTitle = 'MC 壁纸',
    [int]$Pad = 12
)

Add-Type -Namespace Mcwp -Name Cap -MemberDefinition @"
[DllImport("user32.dll", SetLastError=true)] public static extern bool SetProcessDpiAwarenessContext(IntPtr value);
[DllImport("user32.dll", SetLastError=true, CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowW(string cls, string title);
[DllImport("user32.dll", SetLastError=true)] public static extern bool GetWindowRect(IntPtr hwnd, out RECT rect);
[DllImport("user32.dll", SetLastError=true)] public static extern bool IsWindowVisible(IntPtr hwnd);
public struct RECT { public int Left, Top, Right, Bottom; }
"@

# -4 = DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2
[Mcwp.Cap]::SetProcessDpiAwarenessContext([IntPtr]::new(-4)) | Out-Null

Add-Type -AssemblyName System.Drawing

# 面板窗口是枚举出来的:SameTitle 直接用 FindWindow 找不到(标题带 · ),这里全窗口扫一遍。
Add-Type -Namespace Mcwp -Name Enum -MemberDefinition @"
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr param);
[DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr hwnd, System.Text.StringBuilder text, int max);
[DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
public delegate bool EnumProc(IntPtr hwnd, IntPtr param);
"@

$target = [IntPtr]::Zero
$cb = [Mcwp.Enum+EnumProc]{
    param($hwnd, $param)
    $sb = New-Object System.Text.StringBuilder 512
    [void][Mcwp.Enum]::GetWindowTextW($hwnd, $sb, 512)
    $title = $sb.ToString()
    if ($title -like "*$MatchTitle*") {
        [uint32]$proc = 0
        [void][Mcwp.Enum]::GetWindowThreadProcessId($hwnd, [ref]$proc)
        $script:target = $hwnd
        return $false
    }
    return $true
}
[void][Mcwp.Enum]::EnumWindows($cb, [IntPtr]::Zero)

if ($target -eq [IntPtr]::Zero) { Write-Error "没找到标题含 '$MatchTitle' 的窗口"; exit 1 }

$rect = New-Object Mcwp.Cap+RECT
[void][Mcwp.Cap]::GetWindowRect($target, [ref]$rect)
$x = $rect.Left - $Pad
$y = $rect.Top - $Pad
$w = ($rect.Right - $rect.Left) + 2 * $Pad
$h = ($rect.Bottom - $rect.Top) + 2 * $Pad

$bmp = New-Object System.Drawing.Bitmap($w, $h)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($x, $y, 0, 0, (New-Object System.Drawing.Size($w, $h)))
$g.Dispose()
$dir = Split-Path -Parent $Out
if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir | Out-Null }
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
"panel-shot: $Out  hwnd=$target  rect=($($rect.Left),$($rect.Top))-$($rect.Right),$($rect.Bottom)  ${w}x${h}"
