# AGENTS.md — Permanent Architectural & Engineering Contract (`waish`)

Dokumen ini adalah kontrak arsitektur permanen bagi setiap agen coding AI maupun kontributor yang memodifikasi repositori `waish`. Seluruh aturan di bawah ini bersifat **wajib dan mutlak** `( •̀ ω •́ )✧`

---

## 1. Visi Inti: Standalone Sovereign Desktop Companion

`waish` adalah biner Rust tunggal (*standalone*) untuk Linux (Hyprland-first, portabel lintas Sway/KDE/GNOME/Generic) yang beroperasi tanpa ketergantungan pada shell interaktif pengguna (`fish`, `zsh`, `nushell`) maupun skrip Python ad-hoc.

### Pipeline Eksekusi 3-Tier (Zero-Waste RPM)
1. **Tier 0 — Fast-Path Local Router (`src/engine/fast_path.rs`)**:
   - Menangani perintah deterministik tunggal harian (`next`/`skip`/`pause`/`play`/`lagu apa`, `tutup ws N`, `pindah ke ws N`, `cek ram`/`disk`/`suhu`/`baterai`) secara lokal dalam `< 10ms` dengan `0` token LLM dan `0` kuota RPM.
   - Hanya aktif pada kecocokan eksklusif (`^...$`); kalimat majemuk atau analitis wajib dikembalikan `None` agar diteruskan ke LLM.
   - Setiap eksekusi Fast-Path wajib disinkronkan ke `scratchpad.json`.
2. **Tier 1 — One-Shot Composite Recipes (`src/tools/recipes.rs`)**:
   - Tool `execute_recipe` menuntaskan alur multi-langkah dalam **1 panggilan tool atomik** (`create_and_open_document`, `open_file_in_workspace`, `hot_edit_document`, `play_media_direct`, `trigger_scene`).
3. **Tier 2 — Hardened ReAct Loop (`src/cli/repl.rs` & `src/tools/runner.rs`)**:
   - Dilindungi oleh Rust Defensive Interceptors, Inline Course-Correction Gatekeeper (`ConfirmationDecision`), True Interactive PTY, dan Bit-for-Bit HTTP 429 Transparent Replay FSM.

---

## 2. Aturan IPC Native (Zero-Shelling-Out pada Jalur Panas)

1. **Hyprland Direct Unix Socket (`src/tools/desktop/hyprland.rs`)**:
   - Berkomunikasi langsung ke `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket.sock` (command/JSON query) dan `.socket2.sock` (event stream), dengan fallback ke biner `hyprctl` hanya bila soket gagal diakses.
   - **ATURAN KRITIS SOKET**: Setiap transaksi ke `.socket.sock` **WAJIB** melakukan `write_all` -> `shutdown(Write)` -> `read_to_end` -> **`drop(stream)` seketika** agar kompositor Hyprland tidak pernah terkunci/membeku.
   - **Atomic Window Trapper**: Peluncuran aplikasi GUI wajib 100% melalui dispatcher kompositor (`dispatch exec [workspace <ws> silent] <cmd>`) agar proses diinduki kompositor (tidak mati terkena `SIGHUP` saat subshell/PTY selesai), dipantau via event `openwindow>>` pada `.socket2.sock` (timeout adaptif `8s` untuk aplikasi berat seperti OnlyOffice/LibreOffice/Electron/Browser), dan mengembalikan `WindowInfo` final.
   - **OnlyOffice Guard**: Dilarang memaksa `QT_QPA_PLATFORM=wayland` saat meluncurkan `onlyoffice-desktopeditors`.
   - **Suicide Protection**: Penutupan batch (`close_workspaces`) wajib memeriksa rantai PID induk `/proc/<pid>/stat` agar terminal host tempat `waish` berjalan tidak pernah ikut ditutup.
2. **Native D-Bus MPRIS (`src/tools/media.rs`)**:
   - Seluruh pembacaan metadata dan kontrol media (`Play`, `Pause`, `Next`, `Previous`, `OpenUri`) wajib menggunakan crate `zbus` langsung ke session D-Bus (`org.mpris.MediaPlayer2.Player`) tanpa memanggil CLI eksternal `playerctl`.
   - Jika bus `org.mpris.MediaPlayer2.spotify` atau jendela `Spotify` aktif, dilarang mengembalikan status palsu `waiting_for_login`.

---

## 3. Engine Dokumen Universal & Transient Alarm

1. **Universal Document Engine (`src/tools/document.rs`)**:
   - `.xlsx` / `.ods` / `.xls` / `.csv`: Tulis native via `rust_xlsxwriter` dan baca native via `calamine`.
   - `.docx`: Tulis dan baca paragraf/heading/tabel native via `docx-rs` (`default-features = false` untuk menjaga ukuran biner tetap ramping `< 12 MB`).
   - `.pdf`: Ekstraksi teks baca-saja via `pdf-extract` dengan batas keras `16 KB` / `200` baris pertama + `[Truncated]`.
   - Siklus `hot_edit`: Tutup bersih jendela editor yang sedang membuka file target -> tunggu pelepasan file lock -> tulis pembaruan ke disk -> buka ulang di workspace yang sama.
2. **Dedicated Transient Alarm (`src/tools/timer.rs`)**:
   - Gunakan `systemd-run --user` (`waish-alarm-<slug>`) untuk menyetel timer (`--on-calendar` maupun `--on-active`).
   - Pembatalan wajib langsung menggunakan `systemctl --user stop` + `systemctl --user reset-failed`. **Dilarang keras** mencari unit alarm menggunakan `find /` atau menggeledah `~/.config/`.

---

## 4. Penjagaan Defensif Level Biner (Jangan Hanya Mengandalkan Prompt!)

Seluruh penjagaan keselamatan kritis wajib dikunci secara deterministik di kode Rust (`src/tools/runner.rs`, `src/engine/provider.rs`, `src/cli/repl.rs`):

1. **Path Firewall**: Rewrite otomatis `/home/{companion_user_name}` dan ekspansi `~/` ke `$HOME` asli milik OS sebelum eksekusi perintah/file.
2. **POSIX Subshell & Process Group Armor**: Selalu jalankan subshell melalui `/bin/bash -c` atau `/bin/sh -c` di dalam `setpgid(0, 0)` mandiri, lengkap dengan proteksi Anti-Self-Kill (`[p]attern` bracket trick & `--ignore-ancestors` pada `pkill`) agar `waish` tidak pernah membunuh dirinya sendiri.
3. **Ad-Hoc Python & Background GUI Interceptors**: Blokir pembuatan/eksekusi skrip Python sementara di `/tmp/*.py` untuk `.xlsx`/`.docx` serta peluncuran `nohup <gui> &` di `run_shell_command`, arahkan langsung ke `execute_recipe` / `manage_document` / `launch_gui_app`.
4. **Anti-Flailing Post-Verification Guard**: Setelah `execute_recipe`, `launch_gui_app`, atau `manage_workspace(launch_app)` mengembalikan hasil verifikasi IPC, blokir segala percobaan menjalankan `sleep` + `hyprctl clients` + `pgrep` tambahan via shell.
5. **Post-Install Auto Binary Inspection (`src/tools/system_ops.rs`)**: Setiap instalasi paket sukses (`pacman -S` / `yay -S` / `paru -S`), lampirkan hasil `pacman -Ql <pkg>` untuk `/usr/bin/` secara otomatis agar model tidak pernah menebak nama binary.
6. **HTTP 429 Bit-for-Bit Replay (`src/engine/provider.rs`)**: Gunakan `FrozenHttpRequest` dengan counter non-resetting (hard cap maksimal `3` retry + exponential safety buffer `+3s`, `+8s`, `+15s` di atas `Retry-After`) **tanpa** menyuntikkan pesan resume sintetis ke `messages`.
7. **Empty-Promise Stall Guard (`src/cli/repl.rs`)**: Jika model merespons dengan janji tindakan tanpa `tool_calls`, lakukan maksimal `1x` koreksi ringan; bila tetap kosong, tampilkan pemberitahuan status eksplisit agar terminal tidak menggantung diam.

---

## 5. Kebersihan Memori, Privasi, & Kebijakan Visual Open-Source

1. **Memory Throttling & Shadow-Git**:
   - Gunakan `scratchpad.json` untuk catatan sementara sesi aktif.
   - `memory.md` hanya boleh ditulis atas instruksi eksplisit pengguna atau fakta sistem permanen.
   - Mutasi file konfigurasi/teks wajib dicadangkan otomatis oleh `ShadowGit` (`~/.local/share/waish/shadow_repo/`).
2. **Zero-Knowledge Security & Repo Privacy**:
   - **0 Hardcoded Personal Data**: Dilarang keras menanamkan username OS lokal pribadi, alamat IP LAN privat (`192.168.x.x`, `10.x.x.x`, `172.16-31.x.x`), maupun API key/token asli di mana pun dalam kode sumber, skrip, atau dokumentasi.
   - Dilarang menyimpan kredensial login, password, atau kode 2FA pengguna ke memori/file mana pun.
3. **Zero Graphic Emoji Policy (Mutlak)**:
   - Seluruh kode sumber, UI terminal, skrip (`install.sh`, `uninstall.sh`), dan respons pendamping **WAJIB 100% bebas dari emoji grafis berwarna**. Gunakan hanya kaomoji teks murni (contoh: `( •̀ ω •́ )✧`, `( ˘ω˘ )`, `(｡•́︿•̀｡)`).

---

## 6. Quality Gates Wajib Sebelum Selesai / Commit

Setiap perubahan kode wajib meluluskan seluruh gerbang verifikasi berikut tanpa pengecualian:

```bash
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test
./install.sh </dev/null
```
