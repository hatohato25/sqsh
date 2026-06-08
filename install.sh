#!/bin/sh
set -eu

REPO="hatohato25/sqsh"
BINARY_NAME="sqsh"

# OS・アーキテクチャを検出し、サポート対象か確認する
detect_target() {
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Linux)
            case "$arch" in
                x86_64)
                    echo "x86_64-unknown-linux-musl"
                    ;;
                *)
                    echo "error: Unsupported architecture: $arch" >&2
                    echo "       Only x86_64 is currently supported on Linux." >&2
                    exit 1
                    ;;
            esac
            ;;
        Darwin)
            echo "error: macOS detected." >&2
            echo "       Please install sqsh via Homebrew:" >&2
            echo "" >&2
            echo "         brew tap hatohato25/sqsh" >&2
            echo "         brew install sqsh" >&2
            exit 1
            ;;
        *)
            echo "error: Unsupported OS: $os" >&2
            echo "       sqsh currently supports Linux (x86_64) and macOS (via Homebrew)." >&2
            exit 1
            ;;
    esac
}

# GitHub Releases API から最新バージョンを取得する
fetch_latest_version() {
    api_url="https://api.github.com/repos/${REPO}/releases/latest"
    version=$(curl -fsSL "$api_url" | grep '"tag_name"' | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')
    if [ -z "$version" ]; then
        echo "error: Failed to fetch the latest version from GitHub API." >&2
        echo "       URL: $api_url" >&2
        exit 1
    fi
    echo "$version"
}

# インストール先ディレクトリを決定する
# root権限があれば /usr/local/bin、なければ ~/.local/bin を使う
determine_install_dir() {
    if [ "$(id -u)" -eq 0 ]; then
        echo "/usr/local/bin"
    else
        echo "${HOME}/.local/bin"
    fi
}

main() {
    target="$(detect_target)"
    version="$(fetch_latest_version)"
    install_dir="$(determine_install_dir)"

    tarball="${BINARY_NAME}-${version}-${target}.tar.gz"
    base_url="https://github.com/${REPO}/releases/download/${version}"
    tarball_url="${base_url}/${tarball}"
    sha256_url="${base_url}/${tarball}.sha256"

    echo "Installing ${BINARY_NAME} ${version} for ${target}..."
    echo ""

    # 一時ディレクトリに作業ファイルをダウンロードする
    tmpdir="$(mktemp -d)"
    # スクリプト終了時に一時ディレクトリを削除する
    trap 'rm -rf "$tmpdir"' EXIT

    echo "Downloading ${tarball}..."
    curl -fsSL --progress-bar -o "${tmpdir}/${tarball}" "$tarball_url" || {
        echo "error: Failed to download tarball." >&2
        echo "       URL: $tarball_url" >&2
        exit 1
    }

    echo "Downloading checksum..."
    curl -fsSL -o "${tmpdir}/${tarball}.sha256" "$sha256_url" || {
        echo "error: Failed to download checksum file." >&2
        echo "       URL: $sha256_url" >&2
        exit 1
    }

    # sha256チェックサムを検証する
    echo "Verifying checksum..."
    expected_sha256="$(awk '{print $1}' "${tmpdir}/${tarball}.sha256")"
    actual_sha256="$(sha256sum "${tmpdir}/${tarball}" | awk '{print $1}')"

    if [ "$expected_sha256" != "$actual_sha256" ]; then
        echo "error: Checksum verification failed!" >&2
        echo "       Expected: $expected_sha256" >&2
        echo "       Actual:   $actual_sha256" >&2
        exit 1
    fi
    echo "Checksum OK."

    # アーカイブを展開してバイナリをインストールする
    tar -xzf "${tmpdir}/${tarball}" -C "$tmpdir"

    mkdir -p "$install_dir"
    cp "${tmpdir}/${BINARY_NAME}" "${install_dir}/${BINARY_NAME}"
    chmod +x "${install_dir}/${BINARY_NAME}"

    echo ""
    echo "Successfully installed ${BINARY_NAME} ${version} to ${install_dir}/${BINARY_NAME}"

    # ~/.local/bin にインストールした場合はPATH追加を促す
    if [ "$install_dir" = "${HOME}/.local/bin" ]; then
        echo ""
        echo "NOTE: ${install_dir} may not be in your PATH."
        echo "      Add the following line to your shell configuration file"
        echo "      (~/.bashrc, ~/.zshrc, etc.) to make sqsh available:"
        echo ""
        echo "        export PATH=\"\$HOME/.local/bin:\$PATH\""
        echo ""
        echo "      Then reload your shell or run:"
        echo ""
        echo "        source ~/.bashrc   # or ~/.zshrc"
        echo ""
    fi
}

main
