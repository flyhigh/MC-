# 只截控制面板窗口(带留白),用于快速对照 UI 改动。
#
# **用 PrintWindow 抓窗口自己的内容,不用「把窗口提到最前 + 截屏」那一套。** 两个理由:
#   1. 提窗口会干扰用户(而"能稳定提上来"就得用 TOPMOST,那会留下持久样式,见 §11.19);
#   2. 截屏抓到的是"屏幕上现在是什么",别人盖在上面就成了别人的窗口 —— 抓窗口内容跟
#      z 序无关,面板被盖住、被拖到屏幕外也照样抓得到。
# `PW_RENDERFULLCONTENT`(2)是必须的:不加它拿到的是标题栏 + 一块空白 —— 面板的内容是
# DWM 合成的,老式的 PrintWindow 抓不到。
param(
    [Parameter(Mandatory=$true)][string]$Out,
    [string]$MatchTitle = 'MC 壁纸',
    [int]$Pad = 12,
    [string]$PadColor = '#202024'
)

Add-Type -Namespace Mcwp -Name Cap -MemberDefinition @"
[DllImport("user32.dll", SetLastError=true)] public static extern bool SetProcessDpiAwarenessContext(IntPtr value);
[DllImport("user32.dll", SetLastError=true, CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowW(string cls, string title);
[DllImport("user32.dll", SetLastError=true)] public static extern bool GetWindowRect(IntPtr hwnd, out RECT rect);
[DllImport("user32.dll", SetLastError=true)] public static extern bool IsWindowVisible(IntPtr hwnd);
[DllImport("user32.dll", SetLastError=true)] public static extern bool PrintWindow(IntPtr hwnd, IntPtr hdc, uint flags);
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
$winW = $rect.Right - $rect.Left
$winH = $rect.Bottom - $rect.Top

# 窗口内容先抓到一块**正好是窗口大小**的画布上,再垫进留白里。留白纯粹是给眼睛看的边界,
# 所以直接填一个中性的底色 —— 反正这块不是屏幕上真的东西。
$content = New-Object System.Drawing.Bitmap($winW, $winH)
$gc = [System.Drawing.Graphics]::FromImage($content)
$hdc = $gc.GetHdc()
$ok = [Mcwp.Cap]::PrintWindow($target, $hdc, 2)   # PW_RENDERFULLCONTENT
$gc.ReleaseHdc($hdc)
$gc.Dispose()

$w = $winW + 2 * $Pad
$h = $winH + 2 * $Pad
$bmp = New-Object System.Drawing.Bitmap($w, $h)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.Clear([System.Drawing.ColorTranslator]::FromHtml($PadColor))
$g.DrawImage($content, $Pad, $Pad, $winW, $winH)
$g.Dispose()
$content.Dispose()

$dir = Split-Path -Parent $Out
if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir | Out-Null }
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
"panel-shot: $Out  hwnd=$target  rect=($($rect.Left),$($rect.Top))-$($rect.Right),$($rect.Bottom)  ${w}x${h}  printwindow=$ok"
