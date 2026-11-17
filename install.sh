#!/bin/sh
# Install or upgrade moraebot from a GitHub release.
#
#   curl -fsSL https://github.com/useonmn/moraebot/releases/latest/download/install.sh | sh
#
# MORAEBOT_VERSION      release tag such as v0.1.0 (default: latest)
# MORAEBOT_INSTALL_DIR  where the binary goes (default: $HOME/.local/bin)
# MORAEBOT_DOWNLOAD_BASE  release base URL (default: the GitHub releases page)
#
# Exit codes: 0 installed, 1 any failure (nothing is replaced on failure).
set -eu

TARGET="aarch64-apple-darwin"
ASSET="moraebot-$TARGET.tar.gz"
SUMS="SHA256SUMS.txt"

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

    if [ "$(uname -s)" != "Darwin" ] ||
        [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" != "1" ]; then
        fail "$E_PLATFORM" "only Apple Silicon macOS ($TARGET) is supported"
    fi

    for tool in curl tar shasum; do
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
    (cd "$tmp" && printf '%s\n' "$line" | shasum -a 256 -c >/dev/null 2>&1) ||
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
        printf 'A background copy exists: run "moraebot install-launchd" to update it.\n'
    fi

    if [ ! -e "${MORAEBOT_HOME:-$HOME/.moraebot}/config.toml" ]; then
        printf 'Next steps:\n  %s init\n  %s check-config\n  %s install-launchd\nThen send the bot a message in its channel or a DM.\n' \
            "$cmd" "$cmd" "$cmd"
    fi
}

main "$@"
