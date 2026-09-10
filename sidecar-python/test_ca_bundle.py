"""CA 证书持久化测试（P2 任务 9）

PyInstaller onefile 的 cacert.pem 位于 %TEMP%\\_MEIxxxx 临时目录，挂机
电脑的「存储感知/管家类软件」会清理 %TEMP% → TLS 全炸（参考项目
web_server.py:86-110 的真实生产事故）。sidecar 同为 onefile 冻结 +
wxautox4 激活/授权走 HTTPS（requests）——同款暴露面。

install_persistent_ca_bundle 语义：
- 冻结态（sys.frozen）：certifi.where() 的 pem 原子复制（tmp+replace）
  到持久目录，设 REQUESTS_CA_BUNDLE / CURL_CA_BUNDLE / SSL_CERT_FILE
  三 env + monkeypatch certifi.where 指向持久副本
- 非冻结（开发/CI）：直接跳过（返回 False，不动 env）
- 任何失败只 WARN 不炸（证书复制失败时 requests 仍走原 _MEI 路径，
  行为不劣于现状）
"""
import os
import sys

import pytest

import sidecar


def _clear_env(monkeypatch):
    for k in ("REQUESTS_CA_BUNDLE", "CURL_CA_BUNDLE", "SSL_CERT_FILE"):
        monkeypatch.delenv(k, raising=False)


def test_frozen_installs_persistent_bundle(tmp_path, monkeypatch):
    """冻结态：复制 pem 到持久目录 + 三 env + certifi.where 改指"""
    _clear_env(monkeypatch)
    src_pem = tmp_path / "src-cacert.pem"
    src_pem.write_text("FAKE PEM CONTENT")

    fake_certifi = types_mod()
    fake_certifi.where = lambda: str(src_pem)
    monkeypatch.setitem(sys.modules, "certifi", fake_certifi)
    monkeypatch.setattr(sys, "frozen", True, raising=False)

    persistent_dir = tmp_path / "persist"
    ok = sidecar.install_persistent_ca_bundle(persistent_dir=str(persistent_dir))

    assert ok is True, "冻结态 + 源 pem 存在 → 应安装成功"
    dst = persistent_dir / "cacert.pem"
    assert dst.read_text() == "FAKE PEM CONTENT", "pem 应被复制到持久目录"
    for k in ("REQUESTS_CA_BUNDLE", "CURL_CA_BUNDLE", "SSL_CERT_FILE"):
        assert os.environ.get(k) == str(dst), f"{k} 应指向持久副本"
    import certifi

    assert certifi.where() == str(dst), "certifi.where 应被改指持久副本"


def test_non_frozen_skips(tmp_path, monkeypatch):
    """非冻结（开发/CI）：跳过——不动 env、不建目录"""
    _clear_env(monkeypatch)
    monkeypatch.delattr(sys, "frozen", raising=False)
    persistent_dir = tmp_path / "persist"
    ok = sidecar.install_persistent_ca_bundle(persistent_dir=str(persistent_dir))
    assert ok is False, "非冻结态应跳过"
    assert not persistent_dir.exists(), "非冻结态不应创建持久目录"
    assert "REQUESTS_CA_BUNDLE" not in os.environ


def test_copy_failure_warns_not_raises(tmp_path, monkeypatch, capsys):
    """源 pem 不可得（certifi 缺失）→ WARN 不炸，返回 False"""
    _clear_env(monkeypatch)
    monkeypatch.setattr(sys, "frozen", True, raising=False)
    # certifi 不在 sys.modules 且 import 不了
    monkeypatch.setitem(sys.modules, "certifi", None)  # import certifi → ImportError

    ok = sidecar.install_persistent_ca_bundle(persistent_dir=str(tmp_path / "p"))
    assert ok is False, "certifi 缺失应优雅失败"
    out = capsys.readouterr().err
    assert "CA" in out or "证书" in out, f"stderr 应有 WARN 留痕，实际: {out!r}"


def types_mod():
    """构造一个可当 certifi 用的假模块对象"""
    import types

    return types.ModuleType("certifi")
