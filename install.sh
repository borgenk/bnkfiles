#!/usr/bin/env sh
set -eu

# Downloads a release from GitHub and installs it into ~/.local/bin, along with
# the desktop entry so it shows up in your app menu. The download is checked
# against the sha256 published with the release before anything is unpacked.
# Pin a specific version with BNKFILES_VERSION=v0.1.0.

REPO="borgenk/bnkfiles"
APP_ID="io.github.borgenk.BnkFiles"

# Pick the downloader. curl also takes file:// URLs, which is what lets the
# installer test drive this path with no network.
select_fetch() {
    if command -v curl > /dev/null 2>&1; then
        fetch() { curl -fsSL "$1" -o "$2"; }
        fetch_stdout() { curl -fsSL "$1"; }
    elif command -v wget > /dev/null 2>&1; then
        fetch() { wget -q "$1" -O "$2"; }
        fetch_stdout() { wget -q "$1" -O -; }
    else
        echo "Error: curl or wget is required"
        return 1
    fi
}

# The download's sha256, or non-zero when the machine has nothing to compute it
# with.
sha256_of() {
    if command -v sha256sum > /dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum > /dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    elif command -v openssl > /dev/null 2>&1; then
        openssl dgst -sha256 "$1" | awk '{print $NF}'
    else
        return 1
    fi
}

# Check a download against the sha256 published beside it, and refuse when that
# cannot be done: a checksum that will not download and a machine with no digest
# tool look the same as a tampered release.
verify_checksum() {
    file="$1"
    sums_url="$2"

    if [ "${BNKFILES_SKIP_CHECKSUM:-}" = "1" ]; then
        echo "Warning: BNKFILES_SKIP_CHECKSUM=1, installing a download nothing has checked"
        return 0
    fi

    if ! actual="$(sha256_of "$file")"; then
        echo "Error: verifying the download needs sha256sum, shasum or openssl"
        echo "  install one, or set BNKFILES_SKIP_CHECKSUM=1 to install unverified"
        return 1
    fi

    if ! fetch "$sums_url" "$file.sha256" 2> /dev/null; then
        echo "Error: cannot download the checksum for this release"
        echo "  $sums_url"
        echo "  set BNKFILES_SKIP_CHECKSUM=1 to install unverified"
        return 1
    fi

    # The published file is sha256sum output, so the digest is its first field.
    expected="$(cut -d' ' -f1 < "$file.sha256")"
    if [ "${#expected}" -ne 64 ] || [ -n "$(printf '%s' "$expected" | tr -d '0-9a-fA-F')" ]; then
        echo "Error: the published checksum is not a sha256 digest, refusing to install"
        return 1
    fi

    if [ "$expected" != "$actual" ]; then
        echo "Error: checksum mismatch, refusing to install"
        echo "  expected: $expected"
        echo "  actual:   $actual"
        return 1
    fi

    echo "Checksum verified"
}

main() {
    platform="$(uname -s)"
    arch="$(uname -m)"

    case "$platform" in
        Linux)
            case "$arch" in
                x86_64 | x86-64 | x64 | amd64)
                    TARGET="x86_64-unknown-linux-gnu" ;;
                *)
                    echo "Error: unsupported architecture: $arch"
                    exit 1 ;;
            esac
            ;;
        *)
            echo "Error: unsupported OS: $platform (bnkfiles is Linux-only)"
            exit 1
            ;;
    esac

    select_fetch || exit 1

    if [ -n "${BNKFILES_VERSION:-}" ]; then
        VERSION="$BNKFILES_VERSION"
    else
        echo "Resolving latest version..."
        VERSION="$(fetch_stdout "https://api.github.com/repos/${REPO}/releases/latest" \
            | grep '"tag_name"' \
            | head -n1 \
            | cut -d'"' -f4)"
        if [ -z "$VERSION" ]; then
            echo "Error: failed to resolve latest version"
            exit 1
        fi
    fi

    FILENAME="bnkfiles-${VERSION}-${TARGET}.tar.gz"
    URL="https://github.com/${REPO}/releases/download/${VERSION}/${FILENAME}"

    if [ -n "${TMPDIR:-}" ] && [ -d "${TMPDIR}" ]; then
        TMP_DIR="$(mktemp -d "$TMPDIR/bnkfiles-XXXXXX")"
    else
        TMP_DIR="$(mktemp -d "/tmp/bnkfiles-XXXXXX")"
    fi
    trap 'rm -rf "$TMP_DIR"' EXIT

    echo "Downloading bnkfiles ${VERSION} for ${TARGET}..."
    fetch "$URL" "$TMP_DIR/$FILENAME"

    verify_checksum "$TMP_DIR/$FILENAME" "${URL}.sha256" || exit 1

    echo "Extracting..."
    tar -xzf "$TMP_DIR/$FILENAME" -C "$TMP_DIR"

    if [ ! -f "$TMP_DIR/bnkfiles" ]; then
        echo "Error: the archive holds no bnkfiles binary"
        exit 1
    fi

    INSTALL_DIR="${HOME}/.local/bin"
    mkdir -p "$INSTALL_DIR"
    mv "$TMP_DIR/bnkfiles" "$INSTALL_DIR/bnkfiles"
    chmod +x "$INSTALL_DIR/bnkfiles"

    echo "Installed bnkfiles ${VERSION} to ${INSTALL_DIR}/bnkfiles"

    # Desktop entry, bundled in the tarball. Best-effort: a missing piece or
    # absent cache tool never fails the binary install.
    DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
    APPS_DIR="$DATA_HOME/applications"
    if [ -f "$TMP_DIR/${APP_ID}.desktop" ]; then
        mkdir -p "$APPS_DIR"
        cp "$TMP_DIR/${APP_ID}.desktop" "$APPS_DIR/${APP_ID}.desktop"
        update-desktop-database "$APPS_DIR" > /dev/null 2>&1 || true
    fi

    if [ "$(command -v bnkfiles)" = "$INSTALL_DIR/bnkfiles" ]; then
        echo "Run with 'bnkfiles', or open an image from your file manager"
    else
        echo ""
        echo "To run bnkfiles from your terminal, add ~/.local/bin to your PATH:"

        case "${SHELL:-}" in
            *zsh)
                echo "  echo 'export PATH=\$HOME/.local/bin:\$PATH' >> ~/.zshrc"
                echo "  source ~/.zshrc"
                ;;
            *fish)
                echo "  fish_add_path -U \$HOME/.local/bin"
                ;;
            *)
                echo "  echo 'export PATH=\$HOME/.local/bin:\$PATH' >> ~/.bashrc"
                echo "  source ~/.bashrc"
                ;;
        esac
    fi
}

# Sourced as a library by the installer test, run otherwise.
if [ "${BNKFILES_INSTALL_LIB:-}" != "1" ]; then
    main "$@"
fi
