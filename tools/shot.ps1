# 截取整个虚拟桌面(含所有显示器),用 P/Invoke 保证在高 DPI 下取到真实像素。
param([Parameter(Mandatory=$true)][string]$Out)

Add-Type -Namespace Mcwp -Name Native -MemberDefinition @"
[DllImport("user32.dll", SetLastError=true)] public static extern bool SetProcessDpiAwarenessContext(IntPtr value);
[DllImport("user32.dll")] public static extern int GetSystemMetrics(int index);
"@

# -4 = DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2
[Mcwp.Native]::SetProcessDpiAwarenessContext([IntPtr]::new(-4)) | Out-Null

Add-Type -AssemblyName System.Drawing

$x = [Mcwp.Native]::GetSystemMetrics(76)
$y = [Mcwp.Native]::GetSystemMetrics(77)
$w = [Mcwp.Native]::GetSystemMetrics(78)
$h = [Mcwp.Native]::GetSystemMetrics(79)

$bmp = New-Object System.Drawing.Bitmap($w, $h)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($x, $y, 0, 0, (New-Object System.Drawing.Size($w, $h)))
$g.Dispose()
$dir = Split-Path -Parent $Out
if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir | Out-Null }
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
"shot: $Out  region=($x,$y) ${w}x${h}"