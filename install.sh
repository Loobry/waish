#!/usr/bin/env bash
# install.sh - Skrip instalasi otomatis untuk waish
# Zero Graphic Emoji Policy: Hanya menggunakan kaomoji teks murni & format UNIX.

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_DIR="$HOME/.local/bin"
TARGET_BIN="$INSTALL_DIR/waish"
CONFIG_BASE="${XDG_CONFIG_HOME:-$HOME/.config}"
CONFIG_DIR="$CONFIG_BASE/waish"
TARGET_UNINSTALL="$CONFIG_DIR/uninstall.sh"

echo "+-- waish installer ----------------------------------------+"
echo "| Menyiapkan kompilasi dan instalasi waish ke sistem kamu   |"
echo "+-----------------------------------------------------------+"
echo ""

# 0. Pre-Flight Environment Detection (Display Server, Window Manager, Dependencies)
DISPLAY_SERVER="unknown"
CLIPBOARD_PKG=""
CLIPBOARD_BIN=""
SESSION_LOWER="$(printf '%s' "${XDG_SESSION_TYPE:-}" | tr '[:upper:]' '[:lower:]')"
if [ "$SESSION_LOWER" = "wayland" ] || [ -n "${WAYLAND_DISPLAY:-}" ]; then
    DISPLAY_SERVER="wayland"
    CLIPBOARD_PKG="wl-clipboard"
    CLIPBOARD_BIN="wl-paste"
elif [ "$SESSION_LOWER" = "x11" ] || [ -n "${DISPLAY:-}" ]; then
    DISPLAY_SERVER="x11"
    CLIPBOARD_PKG="xclip"
    CLIPBOARD_BIN="xclip"
fi

DESKTOP_LOWER="$(printf '%s' "${XDG_CURRENT_DESKTOP:-}" | tr '[:upper:]' '[:lower:]')"
WM_BACKEND="generic"
if [ -n "${HYPRLAND_INSTANCE_SIGNATURE:-}" ] || printf '%s' "$DESKTOP_LOWER" | grep -q "hyprland"; then
    WM_BACKEND="hyprland"
elif [ -n "${SWAYSOCK:-}" ] || printf '%s' "$DESKTOP_LOWER" | grep -q "sway"; then
    WM_BACKEND="sway"
elif printf '%s' "$DESKTOP_LOWER" | grep -Eq "kde|plasma"; then
    WM_BACKEND="kde"
elif printf '%s' "$DESKTOP_LOWER" | grep -q "gnome"; then
    WM_BACKEND="gnome"
fi

MISSING_PKGS=()
FOUND_PKGS=("zbus-mpris (native)")

if [ -n "$CLIPBOARD_BIN" ]; then
    if command -v "$CLIPBOARD_BIN" >/dev/null 2>&1; then
        FOUND_PKGS+=("$CLIPBOARD_PKG")
    else
        MISSING_PKGS+=("$CLIPBOARD_PKG")
    fi
fi

if command -v curl >/dev/null 2>&1; then
    FOUND_PKGS+=("curl")
else
    MISSING_PKGS+=("curl")
fi

echo "alice@waish  ( ˘ω˘ ) ~>"
echo "Pre-flight deteksi lingkungan desktop selesai!"
echo "  +-- Environment Check ------------------------------------+"
printf "  | Display Server : %-38s |\n" "$DISPLAY_SERVER"
printf "  | Window Manager : %-38s |\n" "$WM_BACKEND"
if [ "${#FOUND_PKGS[@]}" -gt 0 ]; then
    printf "  | Terpasang      : %-38s |\n" "${FOUND_PKGS[*]}"
fi
if [ "${#MISSING_PKGS[@]}" -gt 0 ]; then
    printf "  | Belum ada      : %-38s |\n" "${MISSING_PKGS[*]}"
else
    printf "  | Status Paket   : %-38s |\n" "Semua dependensi inti siap ( •̀ ω •́ )✧"
fi
echo "  +---------------------------------------------------------+"
echo ""

if [ "${#MISSING_PKGS[@]}" -gt 0 ]; then
    echo "alice@waish  (｡•́︿•̀｡) ~>"
    echo "Beberapa paket pendukung belum terpasang: ${MISSING_PKGS[*]}"
    if command -v pacman >/dev/null 2>&1; then
        if [ -t 0 ]; then
            read -r -p "Mau pasang sekarang via pacman (sudo pacman -S --needed ${MISSING_PKGS[*]})? [Y/n]: " install_deps
            case "$install_deps" in
                n|N)
                    echo "  [info] Dilewati. Kamu bisa pasang manual nanti: sudo pacman -S --needed ${MISSING_PKGS[*]}"
                    ;;
                *)
                    sudo pacman -S --needed "${MISSING_PKGS[@]}" || echo "  [warn] Sebagian paket gagal/dilewati, instalasi waish tetap dilanjutkan."
                    ;;
            esac
        else
            echo "  [tip] Di Arch Linux, pasang dengan: sudo pacman -S --needed ${MISSING_PKGS[*]}"
        fi
    else
        echo "  [tip] Silakan pasang paket tersebut lewat package manager distro kamu yaa."
    fi
    echo ""
fi

# 1. Cek keberadaan cargo di sistem
if ! command -v cargo >/dev/null 2>&1; then
    echo "alice@waish  (｡•́︿•̀｡) ~>"
    echo "Hmm, aku nggak nemu 'cargo' (Rust toolchain) di sistem kamu."
    echo "Pasang dulu yaa (contoh di Arch Linux: sudo pacman -S rustup && rustup default stable),"
    echo "habis itu jalankan lagi ./install.sh ini ( •̀ ω •́ )✧"
    exit 1
fi

# 2. Cek ketersediaan editor terminal ($EDITOR atau nano) untuk Setup Wizard & `waish -c`
EDITOR_BIN="${EDITOR%% *}"
HAS_EDITOR=0
if [ -n "$EDITOR_BIN" ] && command -v "$EDITOR_BIN" >/dev/null 2>&1; then
    HAS_EDITOR=1
elif command -v nano >/dev/null 2>&1; then
    HAS_EDITOR=1
fi

if [ "$HAS_EDITOR" -eq 0 ]; then
    echo "alice@waish  (｡•́︿•̀｡) ~>"
    echo "Catatan: Variabel \$EDITOR belum diset dan editor 'nano' belum terpasang di sistem kamu."
    if command -v pacman >/dev/null 2>&1; then
        echo "Setup wizard waish menggunakan editor terminal (default: nano) untuk kustomisasi persona & memori."
        if [ -t 0 ]; then
            read -r -p "Mau pasang 'nano' sekarang lewat pacman (sudo pacman -S nano)? [Y/n]: " install_nano
            case "$install_nano" in
                n|N)
                    echo "  [info] Dilewati. Kamu juga bisa pasang manual nanti dengan: sudo pacman -S nano"
                    ;;
                *)
                    sudo pacman -S --needed nano || echo "  [warn] Instalasi nano dilewati, fallback ke editor lain yang tersedia."
                    ;;
            esac
        else
            echo "  [tip] Pasang nano dengan perintah: sudo pacman -S nano"
        fi
    else
        echo "  [tip] Pasang 'nano' atau set variabel \$EDITOR di shell kamu yaa."
    fi
    echo ""
fi

# 3. Jalankan kompilasi rilis
echo "alice@waish  ( - _ - ) . . . ~>"
echo "Sedang mengompilasi biner rilis (cargo build --release)..."
echo ""

cd "$SCRIPT_DIR"
cargo build --release

if [ ! -f "$SCRIPT_DIR/target/release/waish" ]; then
    echo ""
    echo "alice@waish  (╥﹏╥) ~>"
    echo "Aduh, file target/release/waish nggak ditemukan setelah build."
    exit 1
fi

# 4. Pasang biner ke ~/.local/bin/waish dan salin uninstall.sh ke ~/.config/waish/uninstall.sh
mkdir -p "$INSTALL_DIR"
install -m 755 "$SCRIPT_DIR/target/release/waish" "$TARGET_BIN"

mkdir -p "$CONFIG_DIR"
chmod 700 "$CONFIG_DIR"
if [ -f "$SCRIPT_DIR/uninstall.sh" ]; then
    install -m 755 "$SCRIPT_DIR/uninstall.sh" "$TARGET_UNINSTALL"
fi

echo ""
echo "alice@waish  ( •̀ ω •́ )✧ ~> "
echo "Yatta! waish udah terpasang di ~/.local/bin/waish."
echo "Ketik 'waish' buat mulai ngobrol sama aku yaa!"

# 5. Cek apakah ~/.local/bin sudah ada di $PATH
case ":${PATH}:" in
    *":${INSTALL_DIR}:"*)
        ;;
    *)
        USER_SHELL="$(basename "${SHELL:-sh}")"
        echo ""
        echo "  +-- Catatan PATH -----------------------------------------+"
        echo "  | Folder ~/.local/bin belum masuk ke \$PATH shell kamu.    |"
        case "$USER_SHELL" in
            fish)
                echo "  | Jalankan perintah ini di Fish:                          |"
                echo "  |   fish_add_path ~/.local/bin                            |"
                ;;
            zsh)
                echo "  | Tambahkan baris ini ke ~/.zshrc kamu:                   |"
                echo "  |   export PATH=\"\$HOME/.local/bin:\$PATH\"                  |"
                ;;
            bash|*)
                echo "  | Tambahkan baris ini ke ~/.bashrc kamu:                  |"
                echo "  |   export PATH=\"\$HOME/.local/bin:\$PATH\"                  |"
                ;;
        esac
        echo "  +---------------------------------------------------------+"
        ;;
esac
