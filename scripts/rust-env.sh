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
# 为什么需要这个脚本（本机四个坑，都踩过了）
# ------------------------------------------------------------------
# 1. Rust 以 GNU 工具链安装（x86_64-pc-windows-gnu），不是 MSVC。
#    原因：本机没有 Visual Studio / Build Tools / Windows SDK，
#    而 rustup 的 GNU 工具链自带 mingw 链接器，不需要 VS。
#
# 2. `~/.cargo/bin/` 下的 cargo.exe / rustc.exe 是 rustup 的**代理 shim**，
#    在本机环境下调用会静默失败（没有任何输出，也不报错）。
#    → 因此本脚本直接指向 toolchain 目录下的真实二进制。
#
# 3. cargo 通过 PATH 查找 rustc 时会命中上面那个坏 shim，
#    报 `os error 193（不是有效的 Win32 应用程序）`。
#    → 因此必须显式设置 RUSTC 环境变量。
#
# 4. `windows-*` 系列 crate 会直接调用外部 `dlltool.exe` 生成导入库，
#    而 rustup 把它放在 self-contained 目录里、默认不在 PATH 上。
#    报错形如：error calling dlltool 'dlltool.exe': program not found
#    → 因此必须把 self-contained 目录加进 PATH。
#
# ------------------------------------------------------------------
# 注意
# ------------------------------------------------------------------
# - PATH 里同时放三个目录：
#     1) toolchain/bin                       真 cargo / rustc
#     2) rustlib/.../bin/self-contained      dlltool.exe、ld.exe
#     3) .cargo/bin                          其他工具（实际无效，但保留兼容）
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
# rustup 自带的 mingw 工具（dlltool / ld）—— 没它 windows-* crate 编不过
_DB_RUST_SELF_CONTAINED="/c/Users/项目发起人/.rustup/toolchains/stable-x86_64-pc-windows-gnu/lib/rustlib/x86_64-pc-windows-gnu/bin/self-contained"
# rustup 自带的 dlltool 缺汇编器 as.exe，会报 `dlltool.exe: CreateProcess`。
# 补一个完整的 mingw-w64（WinLibs，通过 winget 装的用户级包）提供 as.exe。
# 安装命令：winget install BrechtSanders.WinLibs.POSIX.UCRT --source winget
_DB_WINLIBS_BIN="/c/Users/项目发起人/AppData/Local/Microsoft/WinGet/Packages/BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe/mingw64/bin"

# 关键：显式指定 rustc，绕开坏掉的 shim
export RUSTC="$_DB_RUST_TC_WIN/bin/rustc.exe"

# 便捷变量
export CARGO="$_DB_RUST_TC_WIN/bin/cargo.exe"
export RUSTUP="C:/Users/you/.cargo/bin/rustup.exe"

# PATH 顺序（实测可用）：
#   1) WinLibs mingw64/bin           完整的 dlltool + as.exe（必须在前，否则 rustc 会挑到坏的那个）
#   2) toolchain/bin                 真 cargo / rustc
#   3) self-contained                rustup 的 ld
#   4) .cargo/bin                    兼容保留
export PATH="$_DB_WINLIBS_BIN:$_DB_RUST_TC_SH/bin:$_DB_RUST_SELF_CONTAINED:$_DB_RUST_TC_SH/lib/rustlib/x86_64-pc-windows-gnu/bin:/c/Users/项目发起人/.cargo/bin:$PATH"

# 关键：禁止 rustc 使用自带的 self-contained 工具链。
# 不加这一条，rustc 会强制用 rustup 那个缺汇编器的 dlltool，
# 编译 windows-* 系列 crate 时报 `dlltool.exe: CreateProcess`。
export RUSTFLAGS="${RUSTFLAGS:-} -C link-self-contained=no"

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
