# -*- mode: python ; coding: utf-8 -*-
# PyInstaller spec：把 sidecar.py + methods.py 冻结成单文件 exe（wxauto-sidecar.exe）
#
# 用法（Windows 打包机 / CI）：
#   pip install pyinstaller wxautox4
#   pyinstaller sidecar-python/wxauto-sidecar.spec --distpath src-tauri/binaries --workpath build/pyinstaller
#
# 关键点：
# - wxautox4 为延迟导入（运行时 wx.init 才 import），PyInstaller 静态分析抓不到
#   → hiddenimports 显式列出（含 utils.useful.check_license）
# - pythoncom/pywin32 COM 初始化同样显式列出
# - console=True：sidecar 是 stdio JSON-RPC 进程，stdout 必须是管道可读
# - one-file 模式：装机免解压；启动稍慢（自解压到 temp）但分发最简
# - mock 模式（WXAUTO_MOCK=1）不触 wxautox4——CI 无微信也能冒烟

a = Analysis(
    ["sidecar.py"],
    pathex=["."],
    binaries=[],
    datas=[],
    hiddenimports=[
        "wxautox4",
        "wxautox4.utils.useful",
        "pythoncom",
        "win32com",
        "comtypes",
    ],
    hookspath=[],
    hooksconfig={},
    runtime_hooks=[],
    excludes=[],
    noarchive=False,
)

pyz = PYZ(a.pure)

exe = EXE(
    pyz,
    a.scripts,
    a.binaries,
    a.datas,
    [],
    name="wxauto-sidecar",
    debug=False,
    bootloader_ignore_signals=False,
    strip=False,
    upx=False,
    upx_exclude=[],
    runtime_tmpdir=None,
    console=True,
    disable_windowed_traceback=False,
    argv_emulation=False,
    target_arch=None,
    codesign_identity=None,
    entitlements_file=None,
)
