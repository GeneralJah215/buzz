#!/usr/bin/env bash
set -euo pipefail

SIDECARS=(buzz-acp buzz-agent buzz-dev-mcp git-credential-nostr buzz)
HOST=$(rustc -vV | sed -n 's|host: ||p')
TARGET=${1:-$HOST}
if [[ "$TARGET" != *windows* ]]; then
    SIDECARS+=(buzz-backend-kubernetes)
    BUILD_HINT="cargo build --release -p buzz-acp -p buzz-agent -p buzz-backend-kubernetes -p buzz-dev-mcp -p git-credential-nostr -p buzz-cli"
else
    BUILD_HINT="cargo build --release -p buzz-acp -p buzz-agent -p buzz-dev-mcp -p git-credential-nostr -p buzz-cli"
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

# BUG-046: an existence test is not enough. `[[ -f ]]` is true for a 0-byte
# file, so a stub in target/release was copied over the real sidecar and
# bundled. Reject anything empty at the source, before it is staged, and name
# it — a build that stages a broken binary has already lost.
missing=()
empty=()
for bin in "${SIDECARS[@]}"; do
    src="$SRC_DIR/${bin}${EXE}"
    if [[ ! -f "$src" ]]; then
        missing+=("${bin}${EXE}")
    elif [[ ! -s "$src" ]]; then
        empty+=("${bin}${EXE}")
    fi
done
if [[ ${#missing[@]} -gt 0 || ${#empty[@]} -gt 0 ]]; then
    [[ ${#missing[@]} -gt 0 ]] && \
        echo "Error: missing release binaries in $SRC_DIR: ${missing[*]}" >&2
    [[ ${#empty[@]} -gt 0 ]] && \
        echo "Error: ZERO-LENGTH release binaries in $SRC_DIR: ${empty[*]}" >&2
    echo "Run '$BUILD_HINT' first." >&2
    echo "Do not create placeholder files to satisfy this check (BUG-046)." >&2
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

# BUG-046: verify what was actually staged, against the same externalBin list
# Tauri will read at bundle time. One implementation of the rule, shared with
# the beforeBundleCommand hook, so the two can never drift apart.
node desktop/scripts/check-sidecar-binaries.mjs "$TARGET"

echo "Sidecars bundled for $TARGET"
