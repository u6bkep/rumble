#!/usr/bin/env bash
# Build a Windows release bundle (cross-compile from Linux via MinGW)
# and package the binaries + DLLs into a zip under `dist/`.
#
# Usage:
#   scripts/build-windows.sh                  # builds rumble-aetna + server + mumble-bridge
#   scripts/build-windows.sh rumble-aetna     # builds only the listed crate(s)
#
# Requires:
#   - x86_64-w64-mingw32-gcc / -objdump (mingw-w64 toolchain)
#   - rustup target x86_64-pc-windows-gnu
#   - zip
#
# Output:
#   dist/rumble-windows-<git-describe>.zip — a single folder containing
#   every required .exe and DLL.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

TARGET="x86_64-pc-windows-gnu"
PROFILE="release"
MINGW_SYSROOT="/usr/x86_64-w64-mingw32/bin"

DEFAULT_CRATES=(rumble-aetna server mumble-bridge)
if [[ $# -gt 0 ]]; then
    CRATES=("$@")
else
    CRATES=("${DEFAULT_CRATES[@]}")
fi

# Translate crate names to the .exe filenames cargo produces. Crates
# with a `[[bin]] name = "..."` override (e.g. `rumble-aetna` -> the
# bin section names it `rumble-aetna`) all happen to match the crate
# name in this workspace, so 1:1 mapping is fine.
crate_exe() {
    case "$1" in
        rumble-aetna)   echo "rumble-aetna.exe" ;;
        server)         echo "server.exe" ;;
        mumble-bridge)  echo "mumble-bridge.exe" ;;
        *)              echo "$1.exe" ;;
    esac
}

# Preflight: tooling.
missing_tools=()
for cmd in x86_64-w64-mingw32-gcc x86_64-w64-mingw32-objdump zip rustup cargo; do
    command -v "$cmd" >/dev/null 2>&1 || missing_tools+=("$cmd")
done
if (( ${#missing_tools[@]} > 0 )); then
    echo "ERROR: missing required tools: ${missing_tools[*]}" >&2
    echo "  Arch/Manjaro:   sudo pacman -S mingw-w64-gcc zip" >&2
    echo "  Debian/Ubuntu:  sudo apt install mingw-w64 zip" >&2
    exit 1
fi

# Preflight: rust target.
if ! rustup target list --installed | grep -qx "$TARGET"; then
    echo "==> Installing Rust target $TARGET"
    rustup target add "$TARGET"
fi

# Build. Pass each crate as a separate `-p` so cargo only builds what
# we ship (the workspace contains deprecated crates we don't want in
# the release bundle).
build_args=(--target "$TARGET" --profile "$PROFILE" --locked)
for c in "${CRATES[@]}"; do
    build_args+=(-p "$c")
done
echo "==> cargo build ${build_args[*]}"
cargo build "${build_args[@]}"

BIN_DIR="target/$TARGET/$PROFILE"

VERSION="$(git describe --always --dirty --tags --long 2>/dev/null || echo unknown)"
BUNDLE="rumble-windows-$VERSION"
STAGE="$BIN_DIR/$BUNDLE"
ZIP_OUT="dist/$BUNDLE.zip"

echo "==> Staging files in $STAGE"
rm -rf "$STAGE"
mkdir -p "$STAGE"

# 1. The .exe per requested crate.
exes_to_probe=()
for c in "${CRATES[@]}"; do
    exe="$(crate_exe "$c")"
    src="$BIN_DIR/$exe"
    if [[ ! -f "$src" ]]; then
        echo "ERROR: expected $src after build but it's missing" >&2
        exit 1
    fi
    cp "$src" "$STAGE/"
    exes_to_probe+=("$STAGE/$exe")
done

# 2. libmpv-2.dll — rumble-video's build.rs drops it next to the
#    binary, so it lives alongside the .exes when rumble-aetna is in
#    the build set. Skip silently for builds that don't pull it in.
if [[ -f "$BIN_DIR/libmpv-2.dll" ]]; then
    cp "$BIN_DIR/libmpv-2.dll" "$STAGE/"
fi

# 3. MinGW runtime DLLs the .exes reference. Rust+MinGW statically
#    links most of libgcc/libstdc++ by default, but specific deps can
#    still pull in libgcc_s_seh-1.dll / libwinpthread-1.dll, so we
#    walk objdump to be safe rather than assume.
echo "==> Probing for MinGW runtime DLLs"
declare -A copied
for exe in "${exes_to_probe[@]}"; do
    while read -r dll; do
        # Only consider DLLs we have in the mingw sysroot — Win32
        # system DLLs (kernel32.dll, advapi32.dll, etc.) ship with
        # Windows and must not be bundled.
        [[ -z "$dll" ]] && continue
        if [[ -z "${copied[$dll]:-}" && -f "$MINGW_SYSROOT/$dll" ]]; then
            echo "  + $dll"
            cp "$MINGW_SYSROOT/$dll" "$STAGE/"
            copied["$dll"]=1
            # Recurse one level to catch transitive deps (e.g.
            # libstdc++ pulling in libgcc_s_seh).
            while read -r tdll; do
                if [[ -n "$tdll" && -z "${copied[$tdll]:-}" && -f "$MINGW_SYSROOT/$tdll" ]]; then
                    echo "  + $tdll (transitive)"
                    cp "$MINGW_SYSROOT/$tdll" "$STAGE/"
                    copied["$tdll"]=1
                fi
            done < <(x86_64-w64-mingw32-objdump -p "$MINGW_SYSROOT/$dll" 2>/dev/null \
                | awk '/DLL Name:/ { print $3 }')
        fi
    done < <(x86_64-w64-mingw32-objdump -p "$exe" 2>/dev/null \
        | awk '/DLL Name:/ { print $3 }')
done

mkdir -p dist
rm -f "$ZIP_OUT"
echo "==> Creating $ZIP_OUT"
ZIP_ABS="$(pwd)/$ZIP_OUT"
( cd "$BIN_DIR" && zip -r "$ZIP_ABS" "$BUNDLE" >/dev/null )

echo
echo "Bundle: $ZIP_OUT"
ls -la "$ZIP_OUT"
echo "Contents:"
( cd "$STAGE" && ls -la )
