#!/usr/bin/env bash
# One-liner installation commands for dotman
# This file contains the actual one-liner commands for easy copying

# For documentation and website usage

# Universal Unix/Linux/macOS one-liner:
# curl -fsSL https://raw.githubusercontent.com/UtsavBalar1231/dotman/main/install/install.sh | bash

# Or with wget:
# wget -qO- https://raw.githubusercontent.com/UtsavBalar1231/dotman/main/install/install.sh | bash

# From source with Cargo (the workspace has an xtask package, so name the package):
# cargo install --git https://github.com/UtsavBalar1231/dotman --locked dotman-cli

# Arch Linux, local build from a checkout:
# cd packaging/arch && makepkg -si

echo "This file contains one-liner installation commands for dotman"
echo "Use these commands for quick installation on various platforms"
echo ""
echo "Universal one-liner:"
echo "  curl -fsSL https://raw.githubusercontent.com/UtsavBalar1231/dotman/main/install/install.sh | bash"
echo ""
echo "For more installation options, see the full documentation."
