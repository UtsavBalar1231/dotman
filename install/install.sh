#!/usr/bin/env bash
# Universal installation script for dotman
# Detects platform and installs using the most appropriate method

set -euo pipefail

# Configuration
REPO_URL="https://github.com/UtsavBalar1231/dotman"
API_URL="https://api.github.com/repos/UtsavBalar1231/dotman"
DOWNLOAD_BASE="${DOTMAN_INSTALL_BASE_URL:-}"
FALLBACK_VERSION="0.1.0"
BINARY_NAME="dot"
INSTALL_DIR="${DOTMAN_INSTALL_DIR:-$HOME/.local/bin}"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m' # No Color

# Logging functions
log_info() {
	echo -e "${BLUE}[INFO]${NC} $1" >&2
}

log_success() {
	echo -e "${GREEN}[SUCCESS]${NC} $1" >&2
}

log_warning() {
	echo -e "${YELLOW}[WARNING]${NC} $1" >&2
}

log_error() {
	echo -e "${RED}[ERROR]${NC} $1" >&2
}

log_bold() {
	echo -e "${BOLD}$1${NC}" >&2
}

# Platform detection
detect_platform() {
	local os arch

	os=$(uname -s | tr '[:upper:]' '[:lower:]')
	arch=$(uname -m)

	case "$arch" in
	x86_64 | amd64)
		arch="x86_64"
		;;
	aarch64 | arm64)
		arch="aarch64"
		;;
	*)
		log_error "Unsupported architecture: $arch"
		exit 1
		;;
	esac

	case "$os" in
	linux)
		echo "linux-$arch"
		;;
	darwin)
		echo "macos-$arch"
		;;
	*)
		log_error "Unsupported operating system: $os"
		exit 1
		;;
	esac
}

# Check if command exists
command_exists() {
	command -v "$1" >/dev/null 2>&1
}

# Get latest release version
get_latest_version() {
	local json="" version=""
	if [[ -n "$DOWNLOAD_BASE" ]]; then
		echo "$FALLBACK_VERSION"
		return
	fi
	if command_exists curl; then
		json=$(curl -fsSL "$API_URL/releases/latest" 2>/dev/null || true)
	elif command_exists wget; then
		json=$(wget -qO- "$API_URL/releases/latest" 2>/dev/null || true)
	fi
	version=$(printf '%s' "$json" | grep -oE '"tag_name":[[:space:]]*"[^"]*"' | head -n1 | cut -d'"' -f4 | sed 's/^v//')
	if [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
		echo "$version"
	else
		echo "$FALLBACK_VERSION"
	fi
}

# Fetch $1 into $2
fetch() {
	if command_exists curl; then
		curl -fsSL "$1" -o "$2"
	elif command_exists wget; then
		wget -q "$1" -O "$2"
	else
		log_error "Neither curl nor wget found. Cannot download release."
		return 1
	fi
}

# Check the tarball in $1 against its line in checksums.txt, so a tampered or truncated download installs nothing
verify_checksum() {
	local dir="$1" archive="$2"
	grep -F "  $archive" "$dir/checksums.txt" >"$dir/expected" || {
		log_error "No checksum for $archive in checksums.txt"
		return 1
	}
	if command_exists sha256sum; then
		(cd "$dir" && sha256sum -c expected >/dev/null)
	elif command_exists shasum; then
		(cd "$dir" && shasum -a 256 -c expected >/dev/null)
	else
		log_error "Neither sha256sum nor shasum is installed; cannot verify the download"
		return 1
	fi
}

# Download and install from GitHub releases
install_from_github() {
	local platform="$1"
	local version

	version=$(get_latest_version)
	log_info "Installing dotman v$version for $platform"

	local target
	case "$platform" in
	linux-*) target="${platform#linux-}-unknown-linux-musl" ;;
	macos-*) target="${platform#macos-}-apple-darwin" ;;
	esac

	local archive="dotman-v$version-$target.tar.gz"
	local base="${DOWNLOAD_BASE:-$REPO_URL/releases/download/v$version}"
	local temp_dir
	temp_dir=$(mktemp -d)

	log_info "Downloading from $base/$archive"
	if ! fetch "$base/$archive" "$temp_dir/$archive" || ! fetch "$base/checksums.txt" "$temp_dir/checksums.txt"; then
		log_error "Download failed"
		rm -rf "$temp_dir"
		return 1
	fi

	if ! verify_checksum "$temp_dir" "$archive"; then
		log_error "Checksum verification failed; nothing was installed"
		rm -rf "$temp_dir"
		return 1
	fi

	tar -xzf "$temp_dir/$archive" -C "$temp_dir"

	mkdir -p "$INSTALL_DIR"

	cp "$temp_dir/$BINARY_NAME" "$INSTALL_DIR/"
	chmod +x "$INSTALL_DIR/$BINARY_NAME"
	binary_path="$INSTALL_DIR/$BINARY_NAME"

	rm -rf "$temp_dir"

	log_success "dotman installed to $binary_path"
}

# Add to PATH
add_to_path() {
	local shell_rc

	# Detect shell and add to PATH
	if [[ -n "${BASH_VERSION:-}" ]]; then
		shell_rc="$HOME/.bashrc"
	elif [[ -n "${ZSH_VERSION:-}" ]]; then
		shell_rc="$HOME/.zshrc"
	elif [[ -n "${FISH_VERSION:-}" ]]; then
		shell_rc="$HOME/.config/fish/config.fish"
	else
		# Default to bashrc
		shell_rc="$HOME/.bashrc"
	fi

	# Check if already in PATH
	if echo "$PATH" | grep -q "$INSTALL_DIR"; then
		log_info "Install directory already in PATH"
		return 0
	fi

	# Add to PATH in shell configuration
	if [[ "$shell_rc" == *"fish"* ]]; then
		echo "set -gx PATH $INSTALL_DIR \$PATH" >>"$shell_rc"
	else
		echo "export PATH=\"$INSTALL_DIR:\$PATH\"" >>"$shell_rc"
	fi

	log_success "Added $INSTALL_DIR to PATH in $shell_rc"
	log_warning "Please restart your shell or run: source $shell_rc"
}

# Verify installation
verify_installation() {
	local binary_path

	if [[ -x "$INSTALL_DIR/$BINARY_NAME" ]]; then
		binary_path="$INSTALL_DIR/$BINARY_NAME"
	elif command_exists "$BINARY_NAME"; then
		binary_path=$(command -v "$BINARY_NAME")
	else
		log_error "Installation verification failed. Binary not found."
		return 1
	fi

	# Test the binary
	if "$binary_path" --version >/dev/null 2>&1; then
		local version
		version=$("$binary_path" --version | awk '{print $2}')
		log_success "Installation verified! dotman $version is ready to use."
		log_info "Try: $BINARY_NAME --help"
		return 0
	else
		log_error "Binary found but not working correctly"
		return 1
	fi
}

# Show completion setup instructions
show_completion_setup() {
	cat <<EOF

${BOLD}SHELL COMPLETION SETUP:${NC}

Enable shell completions for better usability:

${YELLOW}Bash:${NC}
  echo 'eval "\$(dot completion bash)"' >> ~/.bashrc

${YELLOW}Zsh:${NC}
  echo 'eval "\$(dot completion zsh)"' >> ~/.zshrc

${YELLOW}Fish:${NC}
  dot completion fish | source
  # Or permanently:
  dot completion fish > ~/.config/fish/completions/dot.fish

${YELLOW}PowerShell:${NC}
  Add to your profile:
  Invoke-Expression (& dot completion powershell)

EOF
}

# Main installation function
main() {
	log_bold "dotman Installation Script"
	log_info "Detected platform: $(detect_platform)"

	local platform
	platform=$(detect_platform)

	if install_from_github "$platform"; then
		log_success "Installed from GitHub releases"
		add_to_path
	else
		log_error "All installation methods failed"
		exit 1
	fi

	# Verify the installation
	if verify_installation; then
		show_completion_setup

		log_bold "🎉 Installation complete!"
		log_info "Get started with: $BINARY_NAME init"
	else
		log_error "Installation failed verification"
		exit 1
	fi
}

# Handle command line arguments
case "${1:-}" in
--help | -h)
	cat <<EOF
dotman Installation Script

USAGE:
    $0 [OPTIONS]

OPTIONS:
    --help, -h          Show this help message
    --version, -v       Show version information
    --force-github      Force installation from GitHub releases
    --install-dir DIR   Custom installation directory (default: ~/.local/bin)

ENVIRONMENT VARIABLES:
    DOTMAN_INSTALL_DIR       Custom installation directory
    DOTMAN_INSTALL_BASE_URL  Download tarball and checksums.txt from here instead of GitHub

EXAMPLES:
    $0                                      # Auto-detect and install
    $0 --force-github                       # Force GitHub release installation
    DOTMAN_INSTALL_DIR=/usr/local/bin $0    # Install system-wide

EOF
	exit 0
	;;
--version | -v)
	echo "dotman installer v1.0"
	exit 0
	;;
--force-github)
	platform=$(detect_platform)
	install_from_github "$platform"
	add_to_path
	verify_installation
	show_completion_setup
	;;
--install-dir)
	if [[ -z "${2:-}" ]]; then
		log_error "--install-dir requires a directory argument"
		exit 1
	fi
	INSTALL_DIR="$2"
	shift 2
	main
	;;
"")
	main
	;;
*)
	log_error "Unknown option: $1"
	log_info "Use --help for usage information"
	exit 1
	;;
esac
