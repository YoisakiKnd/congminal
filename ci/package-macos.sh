#!/bin/bash
# 本机构建，无网络依赖；产物保存在 target/distributions，不提交到仓库。
set -euo pipefail
cd "$(dirname "$0")/.."
[[ $(uname -s) == Darwin ]] || { echo '需要在 macOS 上打包' >&2; exit 1; }
version=$(awk -F '"' '/^version = / {print $2; exit}' Cargo.toml)
short_version=${version%%-*}
build_version=${version##*.}
arch=$(uname -m)
preview=false
case "${1:-}" in
  '') ;;
  --sftp-preview) preview=true ;;
  *) echo '用法：bash ci/package-macos.sh [--sftp-preview]' >&2; exit 1 ;;
esac
flavor=''
bundle_id=dev.congminal.app
release_notes=docs/releases/alpha.6.md
if $preview; then
  flavor=-sftp-preview
  bundle_id=dev.congminal.preview
  release_notes=docs/releases/sftp-preview.md
fi
name="congminal-${version}${flavor}-macos-${arch}"
root="$PWD/target/distributions/$name"
app="$root/congminal${flavor}.app"
[[ ! -e "$root" ]] || { echo "产物目录已存在：$root" >&2; exit 1; }
if $preview; then
  cargo build --offline --release --features sftp-preview --bin congminal --bin congminal-ssh-helper
else
  cargo build --offline --release --bin congminal
fi
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/congminal "$app/Contents/MacOS/congminal-bin"
if $preview; then cp target/release/congminal-ssh-helper "$app/Contents/MacOS/"; fi
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
<key>CFBundleDisplayName</key><string>congminal$flavor</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>$short_version</string>
<key>CFBundleVersion</key><string>$build_version</string>
<key>LSMinimumSystemVersion</key><string>11.0</string>
<key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
cp "$release_notes" "$root/使用说明.md"
cp assets/congminal.example.toml "$root/congminal.example.toml"
cp assets/fonts/LICENSE-Terminus.txt "$root/LICENSE-Terminus.txt"
cp assets/shell/congminal.zsh assets/shell/congminal.bash "$root/"
printf '版本：%s%s\n架构：%s\n构建时间（UTC）：%s\n来源提交（包含未提交修改）：%s\nSFTP 预览：%s。无 Rust 第三方运行依赖；预览使用系统 OpenSSH 和附带的按需 helper。\n' \
  "$version" "$flavor" "$arch" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$(git rev-parse HEAD)" "$preview" > "$root/BUILD.txt"
plutil -lint "$app/Contents/Info.plist"
# 本机临时签名，不是 Apple 开发者签名或公证。
codesign --force --sign - "$app/Contents/MacOS/congminal-bin"
if $preview; then codesign --force --sign - "$app/Contents/MacOS/congminal-ssh-helper"; fi
codesign --force --sign - "$app"
codesign --verify --deep --strict "$app"
ditto -c -k --sequesterRsrc --keepParent "$root" "$root.zip"
(cd "$(dirname "$root")" && shasum -a 256 "$name.zip" > "$name.zip.sha256")
echo "$root.zip"
