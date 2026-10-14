# 在桌面放一个「MC 壁纸」快捷方式,双击即挂上壁纸并打开控制面板。
#
# 图标用的是 app/crates/mcwp-ui/assets/mcwp-icon.ico(tools/make-icon.py 生成)。
# 指向的是 cargo 的 release 产物:重新构建后不用更新快捷方式,路径不变。
param(
    [string]$Name = 'MC 壁纸',
    [string]$Exe = 'G:\MC壁纸\app\target\release\mcwp-wallpaper.exe',
    [string]$Icon = 'G:\MC壁纸\app\crates\mcwp-ui\assets\mcwp-icon.ico',
    [string]$Arguments = '--panel'
)

foreach ($path in @($Exe, $Icon)) {
    if (-not (Test-Path -LiteralPath $path)) { Write-Error "找不到 $path"; exit 1 }
}

$shell = New-Object -ComObject WScript.Shell
$desktop = $shell.SpecialFolders.Item('Desktop')
if (-not $desktop) { $desktop = [Environment]::GetFolderPath('Desktop') }

$lnk = Join-Path $desktop "$Name.lnk"
$shortcut = $shell.CreateShortcut($lnk)
$shortcut.TargetPath = $Exe
$shortcut.Arguments = $Arguments
$shortcut.WorkingDirectory = Split-Path -Parent $Exe
$shortcut.IconLocation = "$Icon,0"
$shortcut.Description = 'MC 壁纸 · AI 动态壁纸与控制面板'
$shortcut.Save()

"快捷方式: $lnk"
"  目标: $Exe $Arguments"
"  图标: $Icon"