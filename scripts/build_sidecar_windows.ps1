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

# 预检：tkinter 必须可用（wxautox4 动态依赖；构建机没有则冻结包必缺，冒烟才发现就晚了一轮）
python -c "import tkinter; print('tkinter OK', tkinter.TkVersion)"

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

# ── 冒烟三条 ──────────────────────────────────────────────
# 1) MOCK：mock 数据回来（原有，不触 wxautox4）
# 2) 真路径 wx.init：CI 无授权 → 应收「结果帧」（licensed:false）；
#    若收「错误帧」即打包缺陷（缺 pythoncom/comtypes 等导入级依赖）
# 3) 真路径 wx.activate 假码：授权服务器拒绝是业务错，可接受；
#    ModuleNotFoundError 是打包错——本次事故的直接复现路径
$Exe = "$OutDir/wxauto-sidecar-$Triple.exe"

$env:WXAUTO_MOCK = "1"
$resp = '{"id": 1, "method": "wx.get_my_info", "params": {}}' | & $Exe | Select-Object -First 1
Write-Host "smoke[1/3] mock wxid: $resp"
if (-not ($resp -match '"wxid"')) { throw "sidecar exe MOCK 冒烟失败: $resp" }

Remove-Item Env:WXAUTO_MOCK -ErrorAction SilentlyContinue
$resp2 = '{"id": 2, "method": "wx.init", "params": {}}' | & $Exe | Select-Object -First 1
Write-Host "smoke[2/3] init: $resp2"
if (-not ($resp2 -match '"result"')) { throw "真路径 wx.init 应返回结果帧, 实得: $resp2" }
if ($resp2 -match '"error"') { throw "真路径 wx.init 返回错误帧(疑似缺依赖): $resp2" }

$resp3 = '{"id": 3, "method": "wx.activate", "params": {"code": "CI-SMOKE-FAKE"}}' | & $Exe | Select-Object -First 1
Write-Host "smoke[3/3] activate: $resp3"
if ($resp3 -match 'ModuleNotFoundError') { throw "真路径 wx.activate 缺依赖(本次事故形态): $resp3" }
if (-not (($resp3 -match '"error"') -or ($resp3 -match '"result"'))) { throw "activate 应返回 JSON-RPC 帧, 实得: $resp3" }

Write-Host ("exe size: {0:N1} MB" -f ((Get-Item $Exe).Length / 1MB))
Write-Host "OK: $Exe"
