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
# - pythoncom/win32com COM 初始化同样显式列出
# - console=True：sidecar 是 stdio JSON-RPC 进程，stdout 必须是管道可读
# - one-file 模式：装机免解压；启动稍慢（自解压到 temp）但分发最简
# - mock 模式（WXAUTO_MOCK=1）不触 wxautox4——CI 无微信也能冒烟

a = Analysis(
    ["sidecar.py"],
    pathex=["."],
    binaries=[],
    datas=[],
    # wxautox4 传递依赖显式补齐（2026-09-08 真机事故：wxautox4 是编译型
    # wheel，.pyd 内部的 import 静态分析看不见，冻结包缺 requests 导致
    # activate 抛 ModuleNotFoundError）。清单来源 = PyPI wxautox4 41.1.1.post1
    # 的 requires_dist：colorama/comtypes/pillow/psutil/pyperclip/pywin32/
    # requests/sounddevice/tenacity（comtypes 原清单已有）。
    # ⚠ wxautox4 升级后必须对照 PyPI requires_dist 重新核对此清单！
    # hiddenimports 一律用 import 名（pip 发行名如 pillow/pywin32 不可导入，
    # 会 ERROR not found——2026-09-09 CI 首跑实证：pillow→PIL、pywin32 删
    # （真实子模块 pythoncom/win32com 原清单已有））；标准库若被 .pyd 动态
    # 引用也不会被 requires_dist 声明（tkinter，缺它 wx.init 直接
    # ModuleNotFoundError）——CI 真路径冒烟是唯一防线。pywin32 子模块
    # 按需逐个回填（win32process 2026-09-09 实证；win32api/con/gui/
    # clipboard/ui 预防性）。
    hiddenimports=[
        "wxautox4",
        "wxautox4.utils.useful",
        "pythoncom",
        "win32com",
        "comtypes",
        "colorama",
        "PIL",
        "psutil",
        "pyperclip",
        "tkinter",
        "win32process",
        "win32api",
        "win32con",
        "win32gui",
        "win32clipboard",
        "win32ui",
        "requests",
        "sounddevice",
        "tenacity",
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
