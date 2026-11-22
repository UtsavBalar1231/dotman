#!/usr/bin/env bash
set -euo pipefail
umask 022

usage() {
    cat >&2 <<'USAGE'
Usage: bash scripts/package-linux-gui.sh {deb|rpm} BIN_DIR OUTPUT_DIR

BIN_DIR must contain regular dotman and dotman-gui files, release-version
(one line matching this checkout's CLI version), and SHA256SUMS (one lowercase
SHA-256 and filename per binary). The builder creates this local provenance
receipt immediately after locked builds from the same source. Hash verification
checks byte integrity against that trusted receipt, not independent attestation.
Existing output files are never replaced.
USAGE
}

[[ $# == 3 ]] || { usage; exit 2; }
format=$1
input=$2
output=$3
[[ $format == deb || $format == rpm ]] || { usage; exit 2; }
[[ -d $input && -d $output ]] || { echo "Input and output directories must exist" >&2; exit 1; }
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
output=$(cd -- "$output" && pwd -P)

version=$(awk '
    /^\[package\]$/ { package = 1; next }
    /^\[/ { package = 0 }
    package && /^version = "[^"]+"$/ { split($0, parts, "\""); print parts[2] }
' "$root/Cargo.toml")
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "Unsupported CLI version: $version" >&2; exit 1; }
for file in dotman dotman-gui release-version SHA256SUMS; do
    [[ -f $input/$file && ! -L $input/$file ]] || { echo "Not a regular file: $input/$file" >&2; exit 1; }
done
mapfile -t receipt_version < "$input/release-version"
if [[ ${#receipt_version[@]} != 1 || ${receipt_version[0]} != "$version" ]] ||
    ! cmp -s "$input/release-version" <(printf '%s\n' "$version"); then
    echo "Release version receipt does not match $version" >&2; exit 1;
fi
mapfile -t checksums < "$input/SHA256SUMS"
[[ ${#checksums[@]} == 2 ]] || { echo "SHA256SUMS must contain exactly two entries" >&2; exit 1; }
cmp -s "$input/SHA256SUMS" <(printf '%s\n' "${checksums[@]}") || { echo "Invalid SHA256SUMS framing" >&2; exit 1; }
cli_hash='' gui_hash=''
for line in "${checksums[@]}"; do
    [[ $line =~ ^([0-9a-f]{64})\ \ (dotman|dotman-gui)$ ]] || {
        echo "Invalid SHA256SUMS entry" >&2; exit 1;
    }
    case ${BASH_REMATCH[2]} in
        dotman) [[ -z $cli_hash ]] || { echo "Duplicate dotman checksum" >&2; exit 1; }; cli_hash=${BASH_REMATCH[1]} ;;
        dotman-gui) [[ -z $gui_hash ]] || { echo "Duplicate dotman-gui checksum" >&2; exit 1; }; gui_hash=${BASH_REMATCH[1]} ;;
    esac
done
[[ -n $cli_hash && -n $gui_hash ]] || { echo "Missing checksum" >&2; exit 1; }

scratch=$(mktemp -d "$output/.dotman-package.XXXXXXXX")
created_cli=
created_gui=
cleanup() {
    if [[ -n $created_cli ]]; then rm -f -- "$created_cli"; fi
    if [[ -n $created_gui ]]; then rm -f -- "$created_gui"; fi
    rm -rf -- "$scratch"
}
trap cleanup EXIT
# RPM spec macros interpolate this path.
[[ $scratch =~ ^/[a-zA-Z0-9_./-]+$ ]] || { echo "TMPDIR path contains unsupported characters" >&2; exit 1; }
mkdir -p "$scratch/bin"
install -m755 -- "$input/dotman" "$scratch/bin/dotman"
install -m755 -- "$input/dotman-gui" "$scratch/bin/dotman-gui"
printf '%s  %s\n%s  %s\n' "$cli_hash" "$scratch/bin/dotman" "$gui_hash" "$scratch/bin/dotman-gui" | sha256sum --check --status || {
    echo "Binary checksum mismatch" >&2; exit 1;
}

architecture() {
    local header
    header=$(LC_ALL=C readelf -h "$1")
    [[ $header == *"Class:"*"ELF64"* ]] || return 1
    case $header in
        *"Machine:"*"Advanced Micro Devices X86-64"*) echo x86_64 ;;
        *"Machine:"*"AArch64"*) echo aarch64 ;;
        *) return 1 ;;
    esac
}
glibc_floor() {
    local versions
    versions=$(LC_ALL=C readelf --version-info "$1")
    printf '%s\n' "$versions" | grep -oE 'GLIBC_[0-9]+(\.[0-9]+)+' | sed 's/^GLIBC_//' | sort -Vu | tail -n 1
}
arch=$(architecture "$scratch/bin/dotman") || { echo "Unsupported CLI ELF architecture" >&2; exit 1; }
gui_arch=$(architecture "$scratch/bin/dotman-gui") || { echo "Unsupported GUI ELF architecture" >&2; exit 1; }
[[ $arch == "$gui_arch" ]] || { echo "Binary architectures differ" >&2; exit 1; }
cli_glibc=$(glibc_floor "$scratch/bin/dotman") || { echo "Cannot determine CLI glibc floor" >&2; exit 1; }
gui_glibc=$(glibc_floor "$scratch/bin/dotman-gui") || { echo "Cannot determine GUI glibc floor" >&2; exit 1; }
[[ -n $cli_glibc && -n $gui_glibc ]] || { echo "Missing glibc symbol versions" >&2; exit 1; }

cli="$scratch/cli"
gui="$scratch/gui"
install -Dm755 "$scratch/bin/dotman" "$cli/usr/bin/dotman"
install -Dm755 "$scratch/bin/dotman-gui" "$gui/usr/bin/dotman-gui"
install -Dm644 "$root"/man/*.1 -t "$cli/usr/share/man/man1"
install -Dm644 "$root/completions/dotman.bash" "$cli/usr/share/bash-completion/completions/dotman"
install -Dm644 "$root/completions/_dotman" "$cli/usr/share/zsh/site-functions/_dotman"
install -Dm644 "$root/completions/dotman.fish" "$cli/usr/share/fish/vendor_completions.d/dotman.fish"
install -Dm644 "$root/README.md" "$cli/usr/share/doc/dotman-cli/README.md"
install -Dm644 "$root"/docs/*.md -t "$cli/usr/share/doc/dotman-cli/docs"
install -Dm644 "$root/LICENSE" "$cli/usr/share/doc/dotman-cli/LICENSE"
install -Dm644 "$root/LICENSE" "$gui/usr/share/doc/dotman-gui/LICENSE"
install -Dm644 "$root/gui/dist/LICENSE.txt" "$gui/usr/share/doc/dotman-gui/OFL.txt"
install -Dm644 "$root/gui/dist/THIRD-PARTY-NOTICES.txt" "$gui/usr/share/doc/dotman-gui/THIRD-PARTY-NOTICES.txt"
install -Dm644 "$root/packaging/dotman-gui.desktop" "$gui/usr/share/applications/dotman-gui.desktop"
install -Dm644 "$root/gui/icons/dotman-gui.svg" "$gui/usr/share/icons/hicolor/scalable/apps/dotman-gui.svg"
printf '%s  %s\n%s  %s\n' "$cli_hash" "$cli/usr/bin/dotman" "$gui_hash" "$gui/usr/bin/dotman-gui" | sha256sum --check --status || {
    echo "Staged binary checksum mismatch" >&2; exit 1;
}

if [[ $format == deb ]]; then
    case $arch in x86_64) package_arch=amd64 ;; aarch64) package_arch=arm64 ;; esac
    for package in cli gui; do
        if [[ $package == cli ]]; then
            name=dotman-cli; description="Dotfiles manager"; depends="git, libc6 (>= $cli_glibc), libgcc-s1"
        else
            name=dotman-gui; description="Desktop interface for dotman"; depends="dotman-cli (= $version-1), libc6 (>= $gui_glibc), libgcc-s1, libgtk-3-0, libwebkit2gtk-4.1-0"
        fi
        dir="$scratch/$package"
        mkdir -p "$dir/DEBIAN"
        printf 'Package: %s\nVersion: %s-1\nArchitecture: %s\nMaintainer: Utsav Balar <utsavbalar1231@gmail.com>\nDepends: %s\nDescription: %s\n' \
            "$name" "$version" "$package_arch" "$depends" "$description" > "$dir/DEBIAN/control"
        dpkg-deb --root-owner-group --build "$dir" "$scratch/${name}_${version}-1_${package_arch}.deb"
    done
    cli_archive="$scratch/dotman-cli_${version}-1_${package_arch}.deb"
    gui_archive="$scratch/dotman-gui_${version}-1_${package_arch}.deb"
else
    package_arch=$arch
    mkdir -p "$scratch/rpm"/{BUILD,BUILDROOT,RPMS,SOURCES,SPECS,SRPMS,db}
    for package in cli gui; do
        if [[ $package == cli ]]; then
            name=dotman-cli; description="Dotfiles manager"; requires="git, glibc >= $cli_glibc, libgcc"; license=MIT
        else
            name=dotman-gui; description="Desktop interface for dotman"; requires="dotman-cli = $version-1, glibc >= $gui_glibc, libgcc, gtk3, webkit2gtk4.1"; license="MIT AND OFL-1.1"
        fi
        (cd "$scratch/$package" && find usr -type f -printf '/%p\n' | LC_ALL=C sort) > "$scratch/$package.files"
        cat > "$scratch/rpm/SPECS/$name.spec" <<SPEC
Name: $name
Version: $version
Release: 1
Summary: $description
License: $license
URL: https://github.com/UtsavBalar1231/dotman
BuildArch: $package_arch
Requires: $requires
AutoReqProv: no

%description
$description

%install
mkdir -p "%{buildroot}"
cp -a "$scratch/$package/usr" "%{buildroot}/"

%files -f $scratch/$package.files
%defattr(-,root,root,-)
SPEC
        rpmbuild -bb --dbpath "$scratch/rpm/db" --define "_topdir $scratch/rpm" --define '__os_install_post %{nil}' "$scratch/rpm/SPECS/$name.spec"
    done
    cli_archive="$scratch/rpm/RPMS/$package_arch/dotman-cli-$version-1.$package_arch.rpm"
    gui_archive="$scratch/rpm/RPMS/$package_arch/dotman-gui-$version-1.$package_arch.rpm"
fi

cli_output="$output/${cli_archive##*/}"
gui_output="$output/${gui_archive##*/}"
[[ ! -e $cli_output && ! -L $cli_output && ! -e $gui_output && ! -L $gui_output ]] || {
    echo "Output already exists" >&2; exit 1;
}
ln -- "$cli_archive" "$cli_output"
created_cli=$cli_output
ln -- "$gui_archive" "$gui_output"
created_gui=$gui_output
created_cli='' created_gui=''
printf 'Created %s and %s (glibc floors %s and %s)\n' "$cli_output" "$gui_output" "$cli_glibc" "$gui_glibc"
