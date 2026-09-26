#!/usr/bin/env bash
# Linux 打包：构建 release 二进制，strip 后连同 LICENSE 打成 tar.gz。
#
#   OUT_DIR     输出目录（默认 dist）
#   ARCH_LABEL  文件名里的架构后缀（默认 x64）
#   SKIP_BUILD  1 = 跳过 cargo build
#
# 产物：$OUT_DIR/Germal-linux-$ARCH_LABEL.tar.gz，内含 germal 与 LICENSE（无子目录）。
# 二进制必须叫 germal：应用内更新器解包后按当前可执行文件名找新文件来替换。
set -euo pipefail

OUT_DIR="${OUT_DIR:-dist}"
ARCH_LABEL="${ARCH_LABEL:-x64}"
SKIP_BUILD="${SKIP_BUILD:-0}"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if [ "$SKIP_BUILD" != "1" ]; then
  cargo build --release --locked -p germal-app
fi
binary="target/release/germal"
[ -x "$binary" ] || { echo "找不到 $binary" >&2; exit 1; }

stage="$OUT_DIR/linux"
rm -rf "$stage"
mkdir -p "$stage"
cp "$binary" "$stage/germal"
cp LICENSE "$stage/LICENSE"
strip "$stage/germal"
chmod 755 "$stage/germal"

tarball="$OUT_DIR/Germal-linux-$ARCH_LABEL.tar.gz"
rm -f "$tarball"
tar -C "$stage" -czf "$tarball" germal LICENSE
rm -rf "$stage"

echo "已生成 ${tarball}："
tar -tzvf "$tarball"
