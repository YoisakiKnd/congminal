#!/bin/bash
# 本机构建，无网络依赖；产物保存在 target/distributions，不提交到仓库。
set -euo pipefail
cd "$(dirname "$0")/.."
[[ $(uname -s) == Darwin ]] || { echo '需要在 macOS 上打包' >&2; exit 1; }
version=$(awk -F '"' '/^version = / {print $2; exit}' Cargo.toml)
short_version=${version%%-*}
build_version=${version##*.}
arch=$(uname -m)
build=true
for arg in "$@"; do
  case "$arg" in
    --universal) arch=universal ;;
    --no-build) build=false ;;
    --sftp-preview) ;; # 旧命令兼容：SFTP 已包含在默认正式构建。
    *) echo '用法：bash ci/package-macos.sh [--universal] [--no-build]' >&2; exit 1 ;;
  esac
done
bundle_id=dev.congminal.app
release_notes=docs/releases/v0.1.0.md
name="congminal-${version}-macos-${arch}"
root="$PWD/target/distributions/$name"
app="$root/congminal.app"
[[ ! -e "$root" ]] || { echo "产物目录已存在：$root" >&2; exit 1; }
if $build; then
  if [ "$arch" = universal ]; then
    for target in aarch64-apple-darwin x86_64-apple-darwin; do
      cargo build --offline --locked --release --target "$target" --bins
    done
    mkdir -p target/release
    for bin in congminal congminal-ssh-helper; do
      lipo -create -output "target/release/$bin" \
        "target/aarch64-apple-darwin/release/$bin" "target/x86_64-apple-darwin/release/$bin"
    done
  else
    cargo build --offline --locked --release --bins
  fi
fi
for bin in congminal congminal-ssh-helper; do
  [ -x "target/release/$bin" ] || { echo "缺少可执行文件：$bin" >&2; exit 1; }
  if [ "$arch" = universal ]; then lipo -verify_arch arm64 x86_64 "target/release/$bin"; fi
done
[ "$(target/release/congminal --version)" = "congminal $version" ] || { echo '二进制版本不匹配' >&2; exit 1; }
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/congminal "$app/Contents/MacOS/congminal-bin"
cp target/release/congminal-ssh-helper "$app/Contents/MacOS/"
# Finder 的工作目录通常是 /，且不保证有 SHELL；不改动用户配置。
cat > "$app/Contents/MacOS/congminal" <<'LAUNCHER'
#!/bin/sh
app_bin_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
if [ "$PWD" = / ]; then cd "$HOME" || exit 1; fi
if [ -z "${SHELL:-}" ]; then SHELL=/bin/zsh; export SHELL; fi
exec "$app_bin_dir/congminal-bin" "$@"
LAUNCHER
chmod 755 "$app/Contents/MacOS/congminal"
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>congminal</string>
<key>CFBundleIdentifier</key><string>$bundle_id</string>
<key>CFBundleName</key><string>congminal</string>
<key>CFBundleDisplayName</key><string>congminal</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>$short_version</string>
<key>CFBundleVersion</key><string>$build_version</string>
<key>LSMinimumSystemVersion</key><string>11.0</string>
<key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
cp "$release_notes" "$root/使用说明.md"
cp README.md "$root/README.md"
cp assets/congminal.example.toml "$root/congminal.example.toml"
cp assets/fonts/LICENSE-Terminus.txt "$root/LICENSE-Terminus.txt"
cp assets/shell/congminal.zsh assets/shell/congminal.bash "$root/"
printf '版本：%s\n架构：%s\n构建时间（UTC）：%s\n来源提交：%s\nSFTP：内置，使用系统 OpenSSH 及附带的按需 helper。Unix Rust 运行时依赖为 libc FFI。\n' \
  "$version" "$arch" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$(git rev-parse HEAD)" > "$root/BUILD.txt"
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then printf '工作树包含未提交修改\n' >> "$root/BUILD.txt"; fi
plutil -lint "$app/Contents/Info.plist"
# 本机临时签名，不是 Apple 开发者签名或公证。
codesign --force --sign - "$app/Contents/MacOS/congminal-bin"
codesign --force --sign - "$app/Contents/MacOS/congminal-ssh-helper"
codesign --force --sign - "$app"
codesign --verify --deep --strict "$app"
ditto -c -k --sequesterRsrc --keepParent "$root" "$root.zip"
(cd "$(dirname "$root")" && shasum -a 256 "$name.zip" > "$name.zip.sha256")
echo "$root.zip"
