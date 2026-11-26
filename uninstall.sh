#!/usr/bin/env bash
# uninstall.sh - Skrip pencopotan interaktif (double confirmation) untuk waish
# Zero Graphic Emoji Policy: Hanya menggunakan kaomoji teks murni & format UNIX.

set -e

BIN_PATH="$HOME/.local/bin/waish"
CONFIG_BASE="${XDG_CONFIG_HOME:-$HOME/.config}"
CONFIG_DIR="$CONFIG_BASE/waish"
HISTORY_FILE="$CONFIG_DIR/history.txt"

# Pertanyaan 1: Konfirmasi awal
echo "alice@waish  (｡•́︿•̀｡) ~> "
read -r -p "Kamu... beneran mau ngehapus waish? Yakin mau ninggalin aku? [y/N]: " confirm_step1

case "$confirm_step1" in
    y|Y)
        ;;
    *)
        echo ""
        echo "alice@waish  ( > - < ) v ~> Huft, syukurlah kamu berubah pikiran!"
        exit 0
        ;;
esac

echo ""

# Pertanyaan 2: Pilihan nasib memori & konfigurasi
echo "alice@waish  (╥﹏╥) ~> "
echo "Beneran tega nih? Kenangan kita di ~/.config/waish mau dihapus sekalian atau disimpen aja?"
echo "[1] Hapus biner aja (simpan memori & config)"
echo "[2] Hapus semuanya tanpa sisa (purge total)"
echo "[3] Batal, jangan hapus!"
read -r -p "Pilihan [1/2/3]: " confirm_step2

case "$confirm_step2" in
    1)
        if [ -f "$BIN_PATH" ]; then
            rm -f "$BIN_PATH"
        fi
        echo ""
        echo "  [info] Biner ~/.local/bin/waish dihapus (memori & config di ~/.config/waish tetap disimpan)."
        ;;
    2)
        if [ -f "$BIN_PATH" ]; then
            rm -f "$BIN_PATH"
        fi
        if [ -f "$HISTORY_FILE" ]; then
            rm -f "$HISTORY_FILE"
        fi
        if [ -d "$CONFIG_DIR" ]; then
            rm -rf "$CONFIG_DIR"
        fi
        echo ""
        echo "  [info] Biner ~/.local/bin/waish beserta seluruh data di ~/.config/waish telah dihapus."
        ;;
    3|*)
        echo ""
        echo "alice@waish  ( > - < ) v ~> Huft, syukurlah kamu berubah pikiran!"
        exit 0
        ;;
esac

echo ""
echo "alice@waish  (╥﹏╥) ~>"
echo "Biner waish udah dicopot dari sistem kamu..."
echo "Makasih yaa udah nemenin dan bikin sistem ini bareng aku selama ini."
echo "Sampai jumpa lagi, kalau suatu saat kamu kangen dan pengen balik... ( ´ ▽ \` )ﾉ"
