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
# 1) MOCK：mock 数据回来（不触 wxautox4，确定性硬断言）
# 2/3) 真路径：断言重心 = 任何帧里出现 ModuleNotFoundError 即打包缺陷（硬失败）；
#     空响应/超时 = CI 无微信环境下 wxautox4 原生崩溃或弹窗阻塞（环境行为，WARN）。
#     2026-09-09 CI 五跑实证：真 wxautox4 在无微信机器不承诺优雅降级结果帧。
# 帮手：超时防挂（弹窗会把 CI 挂 6 小时）+ stderr 显式回显（保住 [SIDECAR]
# 日志在 CI 的可见性——重定向后不再自动透传）。
$Exe = "$OutDir/wxauto-sidecar-$Triple.exe"

function Invoke-SidecarRpc {
    param([string]$ExePath, [string]$Request, [int]$TimeoutSec = 90)
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $ExePath
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $p = [System.Diagnostics.Process]::Start($psi)
    $p.StandardInput.WriteLine($Request)
    $p.StandardInput.Close()
    $timedOut = -not $p.WaitForExit($TimeoutSec * 1000)
    if ($timedOut) { try { $p.Kill() } catch {} }
    $stdout = try { $p.StandardOutput.ReadToEnd() } catch { "" }
    $stderr = try { $p.StandardError.ReadToEnd() } catch { "" }
    if ($stderr) { Write-Host $stderr }
    $out = if ($stdout) { $stdout.Trim() } else { "" }
    return @{ TimedOut = $timedOut; Out = $out }
}

# 1/3 MOCK
$env:WXAUTO_MOCK = "1"
$r1 = Invoke-SidecarRpc -ExePath $Exe -Request '{"id": 1, "method": "wx.get_my_info", "params": {}}'
Write-Host "smoke[1/3] mock: $($r1.Out)"
if (-not ($r1.Out -match '"wxid"')) { throw "sidecar exe MOCK 冒烟失败: $($r1.Out)" }
Remove-Item Env:WXAUTO_MOCK -ErrorAction SilentlyContinue

# 2/3 真路径 wx.init
$r2 = Invoke-SidecarRpc -ExePath $Exe -Request '{"id": 2, "method": "wx.init", "params": {}}'
Write-Host "smoke[2/3] init (timedOut=$($r2.TimedOut)): $($r2.Out)"
if ($r2.Out -match 'ModuleNotFoundError') { throw "真路径 wx.init 报缺依赖(打包缺陷): $($r2.Out)" }
if ($r2.Out -eq "") {
    Write-Host "WARN: wx.init 无响应帧(超时=$($r2.TimedOut))——CI 无微信环境 wxautox4 原生崩溃/阻塞属环境行为, 非打包缺陷"
} elseif (-not (($r2.Out -match '"result"') -or ($r2.Out -match '"error"'))) {
    throw "wx.init 应返回 JSON-RPC 帧, 实得: $($r2.Out)"
}

# 3/3 真路径 wx.activate 假码（2026-09-08 事故直接复现路径）
$r3 = Invoke-SidecarRpc -ExePath $Exe -Request '{"id": 3, "method": "wx.activate", "params": {"code": "CI-SMOKE-FAKE"}}'
Write-Host "smoke[3/3] activate (timedOut=$($r3.TimedOut)): $($r3.Out)"
if ($r3.Out -match 'ModuleNotFoundError') { throw "真路径 wx.activate 缺依赖(打包缺陷, 2026-09-08 事故形态): $($r3.Out)" }
if ($r3.Out -eq "") {
    Write-Host "WARN: activate 无响应帧(超时=$($r3.TimedOut))——同上环境行为"
} elseif (-not (($r3.Out -match '"result"') -or ($r3.Out -match '"error"'))) {
    throw "activate 应返回 JSON-RPC 帧, 实得: $($r3.Out)"
}

Write-Host ("exe size: {0:N1} MB" -f ((Get-Item $Exe).Length / 1MB))
Write-Host "OK: $Exe"
