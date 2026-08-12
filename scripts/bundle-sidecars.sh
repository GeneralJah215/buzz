#!/usr/bin/env bash
set -euo pipefail

# buzz-edge is listed in tauri.conf.json's externalBin, so it has to be staged
# here too or the bundle step fails on a missing sidecar. Without the
# externalBin entry the installer's IfFileExists check is always false, the
# scheduled-task registration is dead code in every shipped build, and the
# supervisor's repair path can never run (spec acceptance item 17).
SIDECARS=(buzz-acp buzz-agent buzz-dev-mcp git-credential-nostr buzz buzz-edge)
HOST=$(rustc -vV | sed -n 's|host: ||p')
TARGET=${1:-$HOST}
if [[ "$TARGET" != *windows* ]]; then
    SIDECARS+=(buzz-backend-kubernetes)
    BUILD_HINT="cargo build --release -p buzz-acp -p buzz-agent -p buzz-backend-kubernetes -p buzz-dev-mcp -p buzz-edge -p git-credential-nostr -p buzz-cli"
else
    BUILD_HINT="cargo build --release -p buzz-acp -p buzz-agent -p buzz-dev-mcp -p buzz-edge -p git-credential-nostr -p buzz-cli"
fi
BINARIES_DIR="desktop/src-tauri/binaries"

# When --target is passed explicitly to cargo (even if it matches the host),
# binaries land in target/<triple>/release/. Without --target, they land in
# target/release/. The script receives the target as $1 only when cargo was
# invoked with --target, so use the qualified path whenever $1 is set.
if [[ -n "${1:-}" ]]; then
    SRC_DIR="target/${TARGET}/release"
else
    SRC_DIR="target/release"
fi

# MSVC emits <name>.exe; Tauri's externalBin then expects binaries/<name>-<triple>.exe.
if [[ "$TARGET" == *windows* ]]; then
    EXE=".exe"
else
    EXE=""
fi

# A file test alone is not enough. Dropbox recreates 0-byte placeholders in this
# tree (BUG-057), and `-f` is true for every one of them. That is precisely how
# BUG-046 shipped: an installer whose sidecars were all empty, so every agent
# died with "%1 is not a valid Win32 application". The bundle step reported
# success at every stage because nothing ever asked how big the files were.
#
# Smallest real sidecar here is ~2 MB. 100 KB is far below any true binary and
# far above any placeholder, so it separates them without being brittle.
MIN_SIDECAR_BYTES=102400

missing=()
empty=()
for bin in "${SIDECARS[@]}"; do
    src="$SRC_DIR/${bin}${EXE}"
    if [[ ! -f "$src" ]]; then
        missing+=("${bin}${EXE}")
        continue
    fi
    size=$(wc -c <"$src")
    if (( size < MIN_SIDECAR_BYTES )); then
        empty+=("${bin}${EXE} (${size} bytes)")
    fi
done
if [[ ${#missing[@]} -gt 0 ]]; then
    echo "Error: missing release binaries in $SRC_DIR: ${missing[*]}" >&2
    echo "Run '$BUILD_HINT' first." >&2
    exit 1
fi
if [[ ${#empty[@]} -gt 0 ]]; then
    echo "Error: placeholder-sized binaries in $SRC_DIR: ${empty[*]}" >&2
    echo "These are stubs, not binaries. Bundling them produces an installer" >&2
    echo "whose agents all fail with '%1 is not a valid Win32 application'." >&2
    echo "Run '$BUILD_HINT' first." >&2
    exit 1
fi

mkdir -p "$BINARIES_DIR"
for bin in "${SIDECARS[@]}"; do
    destination="$BINARIES_DIR/${bin}-${TARGET}${EXE}"
    cp "$SRC_DIR/${bin}${EXE}" "$destination"

    # cp preserves the mode of an existing destination on macOS. Generated
    # sidecar placeholders may not be executable, so make the bundled Unix
    # binaries executable explicitly.
    if [[ -z "$EXE" ]]; then
        chmod 755 "$destination"
    fi
done

# Verify what actually landed, not what we intended to copy. A copy into this
# tree can be silently reverted by Dropbox sync between the cp and the bundle
# step, and the success message above would still print.
for bin in "${SIDECARS[@]}"; do
    destination="$BINARIES_DIR/${bin}-${TARGET}${EXE}"
    size=$(wc -c <"$destination" 2>/dev/null || echo 0)
    if (( size < MIN_SIDECAR_BYTES )); then
        echo "Error: $destination is ${size} bytes after copy - staging failed." >&2
        exit 1
    fi
    printf '  %-46s %10d bytes\n' "$(basename "$destination")" "$size"
done
echo "Sidecars bundled for $TARGET"
