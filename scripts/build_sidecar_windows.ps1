# Windows: 把 Python sidecar 冻结成 wxauto-sidecar-<triple>.exe 并放到 src-tauri/binaries/
# （tauri externalBin 约定：src-tauri/binaries/<name>-<target-triple>[.exe]）
# 用法: pwsh scripts/build_sidecar_windows.ps1   （在 desktop/ 目录下执行）
$ErrorActionPreference = "Stop"

$DesktopRoot = Split-Path -Parent $PSScriptRoot
Set-Location $DesktopRoot

# Python 环境检查
python --version
python -m pip install --upgrade pip pyinstaller | Out-Null
python -m pip install -r sidecar-python/requirements.txt | Out-Null

# 冻结（dist 输出到临时目录，再改名搬运——避免把不带 triple 的名字留进 binaries/）
python -m PyInstaller `
    --distpath build/sidecar-dist `
    --workpath build/sidecar-work `
    --noconfirm `
    sidecar-python/wxauto-sidecar.spec

# target triple：与 CI windows-latest MSVC x64 一致（tauri.conf externalBin 匹配名）
$Triple = "x86_64-pc-windows-msvc"
$OutDir = "src-tauri/binaries"
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
Copy-Item "build/sidecar-dist/wxauto-sidecar.exe" "$OutDir/wxauto-sidecar-$Triple.exe" -Force

# 冒烟：MOCK 模式发一条 wx.get_my_info，断言 mock 数据回来（不需要微信/授权）
$env:WXAUTO_MOCK = "1"
$resp = '{"id": 1, "method": "wx.get_my_info", "params": {}}' | & "$OutDir/wxauto-sidecar-$Triple.exe" | Select-Object -First 1
Write-Host "smoke resp: $resp"
if (-not ($resp -match '"wxid"')) {
    throw "sidecar exe MOCK 冒烟失败: $resp"
}
Write-Host "OK: $OutDir/wxauto-sidecar-$Triple.exe"
