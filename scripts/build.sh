#!/usr/bin/env bash
# 构建 memory-system Rust 项目
# 用法: ./build.sh

set -e
DIR="$(cd "$(dirname "$0")" && pwd)"
RUSTUP="$HOME/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin"
export PATH="$RUSTUP:$PATH"

echo "==> 清理旧构建..."
cd "$DIR"
cargo clean

echo "==> 构建 release 版本..."
cargo build --release --target x86_64-unknown-linux-musl

echo "==> 复制 binary 到 skill 目录..."
cp target/x86_64-unknown-linux-musl/release/memory-system "$DIR/memory-system"

echo "✅ 构建完成: $DIR/memory-system"
