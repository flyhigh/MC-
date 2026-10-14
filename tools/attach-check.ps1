# 运行一次挂载测试:启动 -> 截图 -> 采样主屏平均色 -> 等退出并打印关键日志。
param(
    [Parameter(Mandatory=$true)][string]$Mode,
    [int]$Seconds = 8,
    [int]$ShotAt = 4,
    [switch]$Dump
)

$exe   = 'G:\MC壁纸\app\target\release\mcwp-wallpaper.exe'
$shots = 'G:\MC壁纸\app\shots'
New-Item -ItemType Directory -Force -Path $shots | Out-Null

$out = Join-Path $shots "$Mode.out.txt"
$err = Join-Path $shots "$Mode.err.txt"
$png = Join-Path $shots "attach-$Mode.png"

$env:RUST_LOG = 'info'
$argList = @('--attach', $Mode, '--seconds', "$Seconds", '--fps', '30', '--stats', '60')
if ($Dump) { $argList += '--dump' }

$p = Start-Process -FilePath $exe -ArgumentList $argList -PassThru -RedirectStandardOutput $out -RedirectStandardError $err -WindowStyle Hidden
Start-Sleep -Seconds $ShotAt
& 'G:\MC壁纸\tools\shot.ps1' -Out $png | Out-Null
Wait-Process -Id $p.Id

Add-Type -AssemblyName System.Drawing
$bmp = [System.Drawing.Image]::FromFile($png)
# 虚拟桌面原点(-2560,-193) -> 主屏(0,0,1920,1080) 在图片里的位置
$ox = 2560; $oy = 193
$sumR = 0; $sumG = 0; $sumB = 0; $n = 0
$stepX = 40; $stepY = 40
for ($x = $ox + 20; $x -lt $ox + 1900; $x += $stepX) {
    for ($y = $oy + 20; $y -lt $oy + 1060; $y += $stepY) {
        $c = $bmp.GetPixel($x, $y)
        $sumR += $c.R; $sumG += $c.G; $sumB += $c.B; $n++
    }
}
$bmp.Dispose()
"mode=$Mode 主屏平均色=({0},{1},{2})  采样={3}" -f [int]($sumR/$n), [int]($sumG/$n), [int]($sumB/$n), $n
Get-Content $out | Select-String -Pattern '挂载|主显示器|fps=' 
Get-Content $err -ErrorAction SilentlyContinue | Select-Object -First 5