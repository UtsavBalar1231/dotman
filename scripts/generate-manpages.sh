#!/usr/bin/env bash
# Generate man pages for dotman using help2man
# This script builds the binary and generates comprehensive man pages

set -euo pipefail

# Colors for output
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m' # No Color

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

# Configuration
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
MAN_DIR="$PROJECT_ROOT/man"
BINARY_PATH="$PROJECT_ROOT/target/release/dotman"
VERSION=$(grep '^version = ' "$PROJECT_ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')

# Create man pages directory
mkdir -p "$MAN_DIR"

log_info "Building dotman binary..."

# Build the binary first
cd "$PROJECT_ROOT"
if ! cargo build --release --bin dotman; then
    log_error "Failed to build dotman binary"
    exit 1
fi

# Verify binary exists
if [[ ! -f "$BINARY_PATH" ]]; then
    log_error "Binary not found at $BINARY_PATH"
    exit 1
fi

# Check if help2man is available
if ! command -v help2man >/dev/null 2>&1; then
    log_error "help2man not found. Please install help2man:"
    log_error "  Ubuntu/Debian: sudo apt install help2man"
    log_error "  Fedora/RHEL: sudo dnf install help2man"
    log_error "  Arch Linux: sudo pacman -S help2man"
    log_error "  macOS: brew install help2man"
    exit 1
fi

log_success "Binary built successfully"

# Generate main man page
log_info "Generating main man page (dot.1)..."

help2man \
    --no-info \
    --name="blazingly fast dotfiles manager" \
    --section=1 \
    --version-string="$VERSION" \
    --help-option="--help" \
    --version-option="--version" \
    --output="$MAN_DIR/dot.1" \
    "$BINARY_PATH"

log_success "Generated main man page: man/dot.1"

# Generate additional documentation files
log_info "Creating additional man page content..."

# Create a more comprehensive man page with examples
cat > "$MAN_DIR/dot.1.additional" << 'EOF'
.SH EXAMPLES
.TP
Initialize a new dotfiles repository:
.B dot init
.TP
Add configuration files to tracking:
.B dot add ~/.vimrc ~/.bashrc
.TP
Commit changes with a message:
.B dot commit -m "Updated vim configuration"
.TP
Show repository status:
.B dot status
.TP
Show short status:
.B dot status --short
.TP
View commit history:
.B dot log
.TP
Checkout a specific commit:
.B dot checkout abc123
.TP
Reset to HEAD:
.B dot reset
.TP
Show differences:
.B dot diff
.TP
Remove files from tracking:
.B dot rm ~/.old_config
.SH CONFIGURATION
dotman uses a configuration file located at ~/.config/dotman/config (TOML format).
.PP
Default configuration location can be overridden with the DOTMAN_CONFIG_PATH environment variable.
.SH ENVIRONMENT VARIABLES
.TP
.B DOTMAN_CONFIG_PATH
Override the default configuration file path.
.TP
.B DOTMAN_REPO_PATH
Override the default repository path (~/.dotman).
.SH FILES
.TP
.B ~/.dotman/
Default dotfiles repository directory.
.TP
.B ~/.config/dotman/config
Configuration file (TOML format).
.TP
.B ~/.dotman/index.bin
Binary index file tracking managed files.
.TP
.B ~/.dotman/commits/
Directory containing commit snapshots.
.SH PERFORMANCE
dotman is optimized for extreme performance:
.IP \[bu] 2
SIMD-accelerated operations for maximum throughput
.IP \[bu] 2
Parallel file processing using all available CPU cores
.IP \[bu] 2
Memory-mapped I/O for efficient large file handling
.IP \[bu] 2
xxHash3 for ultra-fast file hashing (>1GB/s throughput)
.IP \[bu] 2
Sub-millisecond operations for typical repositories
.IP \[bu] 2
Content-based deduplication and Zstd compression
.IP \[bu] 2
Binary index format for instant loading
.SH COMPATIBILITY
dotman provides git-like semantics and commands, making it familiar to developers.
It supports all major platforms: Linux, macOS, and Windows.
.SH AUTHOR
Written by Utsav Balar <utsavbalar1231@gmail.com>.
.SH REPORTING BUGS
Report bugs at: https://github.com/UtsavBalar1231/dotman/issues
.SH SEE ALSO
.BR git (1),
.BR stow (8),
.BR chezmoi (1)
EOF

# Combine the generated man page with additional content
if [[ -f "$MAN_DIR/dot.1" ]]; then
    # Insert additional content before the last few sections
    temp_file=$(mktemp)

    # Split the man page at the AUTHOR or COPYRIGHT section
    awk '
    /^\.SH (AUTHOR|COPYRIGHT|REPORTING BUGS)/ {
        # Insert additional content before author section
        while ((getline line < "'"$MAN_DIR/dot.1.additional"'") > 0) {
            print line
        }
        close("'"$MAN_DIR/dot.1.additional"'")
    }
    { print }
    ' "$MAN_DIR/dot.1" > "$temp_file"

    mv "$temp_file" "$MAN_DIR/dot.1"
    rm -f "$MAN_DIR/dot.1.additional"

    log_success "Enhanced main man page with examples and additional content"
fi

# Verify man page was created
if [[ ! -f "$MAN_DIR/dot.1" ]]; then
    log_error "Failed to generate man page"
    exit 1
fi

# Show man page info
man_size=$(wc -l < "$MAN_DIR/dot.1")
log_success "Generated comprehensive man page with $man_size lines"

# Test the man page
log_info "Testing man page..."
if man "$MAN_DIR/dot.1" >/dev/null 2>&1; then
    log_success "Man page syntax is valid"
else
    log_warning "Man page may have syntax issues"
fi

log_success "Man page generation completed!"

# Show installation instructions
cat << EOF

INSTALLATION INSTRUCTIONS:

Install system-wide (requires root):
  sudo cp man/dot.1 /usr/local/share/man/man1/
  sudo mandb

Install user-local:
  mkdir -p ~/.local/share/man/man1
  cp man/dot.1 ~/.local/share/man/man1/
  mandb ~/.local/share/man

View the man page:
  man dot

  Or directly:
  man man/dot.1

DISTRIBUTION PACKAGING:

The generated man page is ready for inclusion in distribution packages:
- Debian: Install to /usr/share/man/man1/dot.1.gz (gzipped)
- RPM: Install to %{_mandir}/man1/dot.1.gz (gzipped)
- Arch: Install to /usr/share/man/man1/dot.1.gz (gzipped)
- Alpine: Install to /usr/share/man/man1/dot.1 (uncompressed)

EOF
