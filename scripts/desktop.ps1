# Windows 用户级安装/卸载:二进制 -> %LOCALAPPDATA%\Programs,
# 开始菜单快捷方式。图标由 build.rs 内嵌进 exe,快捷方式直接指向 exe。
# 只装主程序;ssh-sftp-probe / pty-probe 是开发探针,不进用户安装。
param(
    [Parameter(Mandatory = $true)] [string]$App,
    [Parameter(Mandatory = $true)] [string]$Id,
    [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'

$binDir = Join-Path $env:LOCALAPPDATA "Programs\$App"
$exe = Join-Path $binDir "$App.exe"
$menuDir = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'
$lnk = Join-Path $menuDir "$App.lnk"

if ($Uninstall) {
    Remove-Item -Force -ErrorAction SilentlyContinue $lnk
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $binDir
    Write-Host "uninstalled $App"
    exit 0
}

New-Item -ItemType Directory -Force -Path $binDir | Out-Null
Copy-Item "target\release\$App.exe" $exe -Force

$shell = New-Object -ComObject WScript.Shell
$shortcut = $shell.CreateShortcut($lnk)
$shortcut.TargetPath = $exe
$shortcut.WorkingDirectory = $binDir
$shortcut.IconLocation = "$exe,0"
$shortcut.Save()

Write-Host "installed $App -> $exe"
