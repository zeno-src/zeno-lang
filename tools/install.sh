#!/bin/sh
# Installs Zeno on macOS and Linux.
#
#     curl -fsSL https://github.com/zeno-src/zeno-lang/releases/latest/download/install.sh | sh
#
# It works out which kind of machine this is, downloads the `zeno` that
# was built for it, checks that the download arrived whole, puts it in
# `~/.zeno`, and adds `~/.zeno/bin` to the PATH. Then it looks for the
# two programs that parts of Zeno hand work to, a C compiler (`zeno
# build`) and git (`zeno fetch`), and offers to install whichever is
# missing. Nothing is installed outside `~/.zeno` without asking.
#
#     sh install.sh --yes          install the missing tools without asking
#     sh install.sh --no-tools     do not look for them at all
#     sh install.sh --uninstall    take Zeno off this machine again
#
# With `curl ... | sh`, options go after `-s --`:
#
#     curl -fsSL .../install.sh | sh -s -- --yes
#
# Three variables change where things come from and go to:
#
#     ZENO_VERSION   another version than the one below
#     ZENO_HOME      another folder than ~/.zeno
#     ZENO_FROM      another place to download from: a URL, or a folder
#                    that holds the archive and SHA256SUMS.txt
#
# docs/install.md walks through it. Windows has a setup program instead
# (tools/installer/zeno.iss).

set -eu

# The version this script installs. It is the version in Cargo.toml:
# tools/release-check.sh refuses a release in which the two differ.
version=0.9.0

version=${ZENO_VERSION:-$version}
home=${ZENO_HOME:-$HOME/.zeno}
from=${ZENO_FROM:-https://github.com/zeno-src/zeno-lang/releases/download/v$version}

# The line this script adds to a shell's startup file ends with this,
# so that it can find the line again and not add it twice.
mark='# added by the Zeno installer'

say() { printf '%s\n' "$*"; }
stop() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

tools=ask
uninstall=no
for option in "$@"; do
    case $option in
        --yes) tools=yes ;;
        --no-tools) tools=no ;;
        --uninstall) uninstall=yes ;;
        -h | --help)
            say "Installs Zeno $version in $home and adds it to the PATH."
            say "  --yes          install a missing C compiler and git without asking"
            say "  --no-tools     do not look for them at all"
            say "  --uninstall    take Zeno off this machine again"
            exit 0
            ;;
        *) stop "there is no option \`$option\`. The options are --yes, --no-tools and --uninstall" ;;
    esac
done

# The startup files a line may have been added to. A shell reads its
# own, so the line goes into the one for the shell this person uses.
startup_files="$HOME/.zshrc $HOME/.bashrc $HOME/.bash_profile $HOME/.profile $HOME/.config/fish/config.fish"

# ------------------------------------------------------------- Uninstall

if [ "$uninstall" = yes ]; then
    for file in $startup_files; do
        if [ -f "$file" ] && grep -qF "$mark" "$file"; then
            grep -vF "$mark" "$file" > "$file.zeno-tmp" || true
            cat "$file.zeno-tmp" > "$file"
            rm -f "$file.zeno-tmp"
            say "Took the Zeno line out of $file"
        fi
    done
    if [ -f "$home/bin/zeno" ]; then
        rm -rf "$home"
        say "Removed $home"
    elif [ -e "$home" ]; then
        # Only a folder this script filled is removed.
        say "$home has no Zeno in it, so it was left as it is"
    fi
    say "Zeno is uninstalled. The C compiler and git, if this script installed them, were left."
    exit 0
fi

# ------------------------------------------------------ Which machine is it

case $(uname -s) in
    Linux) system=linux ;;
    Darwin) system=macos ;;
    *) stop "this script installs Zeno on macOS and Linux, and this is $(uname -s). On Windows, download the setup program from the releases page" ;;
esac

case $(uname -m) in
    x86_64 | amd64) processor=x86_64 ;;
    aarch64 | arm64) processor=arm64 ;;
    armv7* | armv8l) processor=armv7 ;;
    armv6*) processor=armv6 ;;
    i386 | i486 | i586 | i686) processor=x86 ;;
    *) stop "there is no Zeno built for a $(uname -m) processor. It can be built from its source with Rust: see the README" ;;
esac

# A 32-bit system on a 64-bit ARM processor (a Raspberry Pi with the
# 32-bit system on it) runs the 32-bit Zeno.
if [ "$system-$processor" = linux-arm64 ] && have getconf && [ "$(getconf LONG_BIT 2>/dev/null)" = 32 ]; then
    processor=armv7
fi

case $system-$processor in
    macos-x86_64 | macos-arm64 | linux-*) ;;
    *) stop "there is no Zeno built for $system on a $processor processor" ;;
esac

name=zeno-$version-$system-$processor
archive=$name.tar.gz

# ------------------------------------------------------------ The download

# `fetch <name> <to>` gets one file of the release.
case $from in
    http://* | https://* | file://*)
        if have curl; then
            fetch() { curl -fsSL "$from/$1" -o "$2"; }
        elif have wget; then
            fetch() { wget -q "$from/$1" -O "$2"; }
        else
            stop "downloading needs curl or wget, and this machine has neither"
        fi
        ;;
    *) fetch() { cp "$from/$1" "$2"; } ;;
esac

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

say "Zeno $version for $system on $processor"
say "  from $from"
fetch "$archive" "$work/$archive" || stop "could not get $from/$archive. Is there a release $version, and can this machine reach it?"
fetch SHA256SUMS.txt "$work/SHA256SUMS.txt" || stop "could not get $from/SHA256SUMS.txt, which says what the download should be"

# A download that was cut short, or changed on the way, is not unpacked.
if have sha256sum; then
    found=$(sha256sum "$work/$archive" | cut -d ' ' -f 1)
elif have shasum; then
    found=$(shasum -a 256 "$work/$archive" | cut -d ' ' -f 1)
else
    stop "checking the download needs sha256sum or shasum, and this machine has neither"
fi
wanted=$(grep " [ *]\{0,1\}$archive\$" "$work/SHA256SUMS.txt" | cut -d ' ' -f 1 | head -1)
[ -n "$wanted" ] || stop "SHA256SUMS.txt has no line for $archive"
[ "$found" = "$wanted" ] || stop "the download is not what was released (its checksum is $found, and it should be $wanted). Nothing was installed"
say "  the download is whole"

# ---------------------------------------------------------------- Install

tar -xzf "$work/$archive" -C "$work"
[ -f "$work/$name/zeno" ] || stop "the archive has no \`zeno\` in it"

# An older Zeno in the same place is replaced, whole. A folder that
# holds something else is not: `ZENO_HOME=$HOME` must not empty a home.
if [ -e "$home" ] && [ ! -f "$home/bin/zeno" ] && [ -n "$(ls -A "$home" 2>/dev/null)" ]; then
    stop "$home is not empty and has no Zeno in it. Name another folder with ZENO_HOME"
fi
rm -rf "$home"
mkdir -p "$home/bin"
mv "$work/$name/zeno" "$home/bin/zeno"
chmod +x "$home/bin/zeno"
# Zeno built for a browser, which `zeno build --web` looks for beside `zeno`.
if [ -f "$work/$name/zeno.wasm" ]; then
    mv "$work/$name/zeno.wasm" "$home/bin/zeno.wasm"
fi
# The docs, the examples and the editor setup.
for thing in "$work/$name"/*; do
    [ -e "$thing" ] || continue
    mv "$thing" "$home/"
done

said=$("$home/bin/zeno" --version) || stop "$home/bin/zeno does not run on this machine"
say "  installed $said in $home"

# ------------------------------------------------------------------ PATH

# The line goes into the startup file of the shell this person uses,
# which `SHELL` names. It is written with `$HOME` when Zeno is in the
# usual place, so that it still reads right if the home folder moves.
case $home in
    "$HOME"/*) shown="\$HOME/${home#"$HOME"/}/bin" ;;
    *) shown="$home/bin" ;;
esac

on_path=no
case ":$PATH:" in *":$home/bin:"*) on_path=yes ;; esac

case ${SHELL:-} in
    */zsh) startup=$HOME/.zshrc; line="export PATH=\"$shown:\$PATH\"  $mark" ;;
    */bash)
        # On macOS a new terminal reads .bash_profile, and on Linux .bashrc.
        if [ "$system" = macos ]; then startup=$HOME/.bash_profile; else startup=$HOME/.bashrc; fi
        line="export PATH=\"$shown:\$PATH\"  $mark"
        ;;
    */fish) startup=$HOME/.config/fish/config.fish; line="set -gx PATH \"$shown\" \$PATH  $mark" ;;
    *) startup=$HOME/.profile; line="export PATH=\"$shown:\$PATH\"  $mark" ;;
esac

if [ -f "$startup" ] && grep -qF "$mark" "$startup"; then
    say "  $startup already puts it on the PATH"
elif [ "$on_path" = yes ]; then
    say "  $home/bin is already on the PATH"
else
    mkdir -p "$(dirname "$startup")"
    printf '\n%s\n' "$line" >> "$startup"
    say "  added it to the PATH, in $startup"
fi

# ----------------------------------------------------------------- Tools

# `ask "question"` is true if the answer is yes. A script that is piped
# in from curl has the script on its input, so the question is asked of
# the terminal itself. With no terminal, the answer is no.
ask() {
    [ "$tools" = yes ] && return 0
    if ! (: < /dev/tty) 2>/dev/null; then return 1; fi
    printf '%s [Y/n] ' "$1" > /dev/tty
    read -r answer < /dev/tty || return 1
    case $answer in '' | y | Y | yes | Yes) return 0 ;; *) return 1 ;; esac
}

# `offer <what is missing> <what needs it> <the command>`: say what is
# missing, and install it if the answer is yes.
offer() {
    say ""
    say "$2 needs $1, and this machine has none."
    if [ -z "$3" ]; then
        say "  Install it with your system's package manager."
    elif ask "  Install it now, with \`$3\`?"; then
        # Not being able to install a tool is no reason to say that Zeno
        # itself was not installed.
        $3 || say "  That did not work. Zeno is installed all the same."
    else
        say "  To install it later: $3"
    fi
}

if [ "$tools" = no ]; then
    :
elif [ "$system" = macos ]; then
    # Apple's command line tools hold both: clang, which is also `cc`,
    # and git. macOS has a `cc` and a `git` without them too, which do
    # nothing but offer to install them, so those are not looked for.
    if ! xcode-select -p >/dev/null 2>&1; then
        offer "Apple's command line tools (a C compiler and git)" "\`zeno build\` and \`zeno fetch\`" "xcode-select --install"
    fi
else
    # What installs a package on this system. The first word is `sudo`
    # where that has to be done as root and this is not root.
    as_root=
    if [ "$(id -u)" != 0 ]; then as_root="sudo "; fi
    install=
    compiler=gcc
    if have apt-get; then install="${as_root}apt-get install -y"; compiler="gcc libc6-dev"
    elif have dnf; then install="${as_root}dnf install -y"
    elif have yum; then install="${as_root}yum install -y"
    elif have pacman; then install="${as_root}pacman -S --noconfirm"
    elif have zypper; then install="${as_root}zypper install -y"
    elif have apk; then install="${as_root}apk add"; compiler=build-base
    fi
    if ! have cc && ! have gcc && ! have clang; then
        offer "a C compiler" "\`zeno build\`" "${install:+$install $compiler}"
    fi
    if ! have git; then
        offer "git" "\`zeno fetch\`" "${install:+$install git}"
    fi
fi

# ------------------------------------------------------------------ Done

say ""
if [ "$on_path" = yes ]; then
    say "Zeno is installed. Try it:"
else
    say "Zeno is installed. Open a new terminal, so that it is on the PATH, and try it:"
fi
say ""
say "    zeno run $home/examples/hello.zeno"
say "    zeno new first"
say ""
say "The docs are in $home/docs. To take Zeno off again, run this installer with --uninstall."
