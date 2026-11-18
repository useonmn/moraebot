#!/bin/sh
# Install or upgrade moraebot from a GitHub release (Apple Silicon macOS, Linux x86_64 or aarch64).
#
#   curl -fsSL https://github.com/useonmn/moraebot/releases/latest/download/install.sh | sh
#
# MORAEBOT_VERSION      release tag such as v0.1.0 (default: latest)
# MORAEBOT_INSTALL_DIR  where the binary goes (default: $HOME/.local/bin)
# MORAEBOT_DOWNLOAD_BASE  release base URL (default: the GitHub releases page)
#
# Exit codes: 0 installed, 1 any failure (nothing is replaced on failure).
set -eu

SUMS="SHA256SUMS.txt"
PLATFORMS="supported: Apple Silicon macOS, Linux x86_64 or aarch64 with glibc"

E_VERSION="E_VERSION"
E_PLATFORM="E_PLATFORM"
E_MISSING_TOOL="E_MISSING_TOOL"
E_TEMP_DIR="E_TEMP_DIR"
E_INSTALL_DIR="E_INSTALL_DIR"
E_DOWNLOAD="E_DOWNLOAD"
E_CHECKSUM="E_CHECKSUM"
E_ARCHIVE="E_ARCHIVE"
E_SMOKE="E_SMOKE"
E_REPLACE="E_REPLACE"

tmp=""
new=""

fail() {
    printf 'moraebot install error: %s: %s\n' "$1" "$2" >&2
    exit 1
}

cleanup() {
    if [ -n "$new" ]; then
        rm -f "$new"
    fi
    if [ -n "$tmp" ]; then
        rm -rf "$tmp"
    fi
}

download() {
    curl --proto '=https,file' --tlsv1.2 -fsSL -o "$2" "$1" ||
        fail "$E_DOWNLOAD" "could not download $1"
}

# Sets TARGET, ASSET, SHA256 and SERVICE for this machine, or fails with E_PLATFORM.
detect_platform() {
    case "$(uname -s)" in
        Darwin)
            [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" = "1" ] ||
                fail "$E_PLATFORM" "$PLATFORMS"
            TARGET="aarch64-apple-darwin"
            SHA256="shasum -a 256"
            SERVICE="install-launchd"
            ;;
        Linux)
            case "$(uname -m)" in
                x86_64 | amd64) TARGET="x86_64-unknown-linux-gnu" ;;
                aarch64 | arm64) TARGET="aarch64-unknown-linux-gnu" ;;
                *) fail "$E_PLATFORM" "$PLATFORMS" ;;
            esac
            if command -v sha256sum >/dev/null 2>&1; then
                SHA256="sha256sum"
            else
                SHA256="shasum -a 256"
            fi
            SERVICE="install-systemd"
            ;;
        *) fail "$E_PLATFORM" "$PLATFORMS" ;;
    esac
    ASSET="moraebot-$TARGET.tar.gz"
}

main() {
    version="${MORAEBOT_VERSION:-latest}"
    install_dir="${MORAEBOT_INSTALL_DIR:-$HOME/.local/bin}"
    base="${MORAEBOT_DOWNLOAD_BASE:-https://github.com/useonmn/moraebot/releases}"

    if [ "$version" = "latest" ]; then
        url="$base/latest/download"
    else
        case "$version" in
            *[!v0-9.]*) fail "$E_VERSION" "MORAEBOT_VERSION must look like v1.2.3, got: $version" ;;
        esac
        printf '%s\n' "$version" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$' ||
            fail "$E_VERSION" "MORAEBOT_VERSION must look like v1.2.3, got: $version"
        url="$base/download/$version"
    fi

    detect_platform

    for tool in curl tar "${SHA256%% *}"; do
        command -v "$tool" >/dev/null 2>&1 || fail "$E_MISSING_TOOL" "$tool is required"
    done

    trap cleanup EXIT
    trap 'exit 1' INT TERM
    tmp="$(mktemp -d)" || fail "$E_TEMP_DIR" "could not create a temporary folder"

    mkdir -p "$install_dir" || fail "$E_INSTALL_DIR" "could not create $install_dir"

    download "$url/$ASSET" "$tmp/$ASSET"
    download "$url/$SUMS" "$tmp/$SUMS"

    line="$(grep "  $ASSET\$" "$tmp/$SUMS")" ||
        fail "$E_CHECKSUM" "$SUMS has no entry for $ASSET"
    (cd "$tmp" && printf '%s\n' "$line" | $SHA256 -c >/dev/null 2>&1) ||
        fail "$E_CHECKSUM" "checksum mismatch for $ASSET; nothing was installed"

    tar -xzf "$tmp/$ASSET" -C "$tmp" moraebot ||
        fail "$E_ARCHIVE" "$ASSET does not contain moraebot"

    new="$install_dir/.moraebot.new.$$"
    cp "$tmp/moraebot" "$new" || fail "$E_REPLACE" "could not write $new"
    chmod 755 "$new" || fail "$E_REPLACE" "could not make $new executable"
    installed="$("$new" --version </dev/null)" ||
        fail "$E_SMOKE" "the downloaded moraebot does not run; nothing was installed"
    mv -f "$new" "$install_dir/moraebot" ||
        fail "$E_REPLACE" "could not replace $install_dir/moraebot"
    new=""

    printf 'Installed %s to %s\n' "$installed" "$install_dir/moraebot"

    case ":${PATH:-}:" in
        *":$install_dir:"*) cmd="moraebot" ;;
        *)
            cmd="$install_dir/moraebot"
            case "${SHELL:-}" in
                */zsh) rc="~/.zshrc" ;;
                */bash) rc="~/.bash_profile" ;;
                *) rc="your shell startup file" ;;
            esac
            printf 'Add this line to %s to put moraebot on your PATH:\n  export PATH="%s:$PATH"\n' \
                "$rc" "$install_dir"
            ;;
    esac

    if [ -e "${MORAEBOT_HOME:-$HOME/.moraebot}/bin/moraebot" ]; then
        printf 'A background copy exists: run "moraebot %s" to update it.\n' "$SERVICE"
    fi

    if [ ! -e "${MORAEBOT_HOME:-$HOME/.moraebot}/config.toml" ]; then
        printf 'Next steps:\n  %s init\n  %s check-config\n  %s %s\nThen send the bot a message in its channel or a DM.\n' \
            "$cmd" "$cmd" "$cmd" "$SERVICE"
    fi
}

main "$@"
