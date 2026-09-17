#!/usr/bin/env bash
# ============================================================
# DeskBase · Rust 工具链环境
# ============================================================
# 用法：在跑任何 cargo / rustc 命令前先 source 本文件
#
#     source scripts/rust-env.sh
#     "$CARGO" build --release
#
# ------------------------------------------------------------------
# 为什么需要这个脚本（本机三个坑，都踩过了）
# ------------------------------------------------------------------
# 1. Rust 以 GNU 工具链安装（x86_64-pc-windows-gnu），不是 MSVC。
#    原因：本机没有 Visual Studio / Build Tools / Windows SDK，
#    而 rustup 的 GNU 工具链自带 rust-mingw 链接器，不需要 VS。
#
# 2. `~/.cargo/bin/` 下的 cargo.exe / rustc.exe 是 rustup 的**代理 shim**，
#    在本机环境下调用会静默失败（没有任何输出，也不报错）。
#    → 因此本脚本直接指向 toolchain 目录下的真实二进制。
#
# 3. cargo 通过 PATH 查找 rustc 时会命中坏的 shim，
#    报 `os error 193（不是有效的 Win32 应用程序）`。
#    → 因此必须显式设置 RUSTC 环境变量。
#
# ------------------------------------------------------------------
# 注意
# ------------------------------------------------------------------
# - PATH 里同时放两个目录：toolchain 的 bin（真二进制）+ .cargo/bin（其他工具）
#   toolchain 的 bin 必须在前。
# - 本脚本不改用户的系统 PATH，不影响其他项目。
# ============================================================

# 防止重复 source
if [ -n "$_DESKBASE_RUST_ENV_LOADED" ]; then
  return 0 2>/dev/null || exit 0
fi
_DESKBASE_RUST_ENV_LOADED=1

export RUSTUP_HOME="C:/Users/you/.rustup"
export CARGO_HOME="C:/Users/you/.cargo"

# toolchain 真实二进制所在目录（Windows 路径形式，供 RUSTC 用）
_DB_RUST_TC_WIN="C:/Users/you/.rustup/toolchains/stable-x86_64-pc-windows-gnu"
# 同一目录的 Git Bash 路径形式（供 PATH 用）
_DB_RUST_TC_SH="/c/Users/项目发起人/.rustup/toolchains/stable-x86_64-pc-windows-gnu"

# 关键：显式指定 rustc，绕开坏掉的 shim
export RUSTC="$_DB_RUST_TC_WIN/bin/rustc.exe"

# 便捷变量
export CARGO="$_DB_RUST_TC_WIN/bin/cargo.exe"
export RUSTUP="C:/Users/you/.cargo/bin/rustup.exe"

# PATH：toolchain bin 必须在最前
export PATH="$_DB_RUST_TC_SH/bin:/c/Users/项目发起人/.cargo/bin:$PATH"

# 校验
_db_rust_check() {
  echo "RUSTUP_HOME = $RUSTUP_HOME"
  echo "CARGO_HOME  = $CARGO_HOME"
  echo "RUSTC       = $RUSTC"
  echo "CARGO       = $CARGO"
  echo -n "cargo 版本  : "
  "$CARGO" -V 2>&1
  echo -n "rustc 版本  : "
  "$RUSTC" --version 2>&1
}
