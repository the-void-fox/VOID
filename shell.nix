# Воспроизводимое окружение разработки VOID.
#
# Использование:
#   nix-shell              — войти в shell
#   nix-shell --run "..."  — выполнить одну команду в окружении
#
# Что внутри (из nixpkgs, ставится из бинарного кэша cache.nixos.org):
#   - QEMU (qemu-system-riscv64) — запуск ядра
#   - gdb — отладка через QEMU gdbstub
#   - rust-analyzer — LSP
#
# Почему Rust НЕ из nix:
#   В этой системе nix-песочница не имеет DNS, а пользователь не trusted, поэтому
#   fixed-output fetch тулчейна (rust-overlay) внутри песочницы не скачивается.
#   Зато host-сеть и rustup работают. Поэтому Rust берём из rustup декларативно
#   через Code/rust-toolchain.toml (там же запинен канал и target riscv64gc-unknown-none-elf,
#   который rustup доустановит сам при первой сборке).

let
  pkgs = import <nixpkgs> { };
in
pkgs.mkShell {
  name = "void-dev";

  packages = [
    pkgs.qemu
    pkgs.gdb
    pkgs.rust-analyzer
    pkgs.grub2
    pkgs.xorriso
    pkgs.util-linux
    pkgs.mtools
  ];

  shellHook = ''
    echo ""
    echo "  ┌─ VOID dev-shell ────────────────────────────────"
    echo "  │ rust:   $(rustc --version 2>/dev/null || echo '— rustup не найден на PATH')"
    echo "  │ target: riscv64gc-unknown-none-elf (из rust-toolchain.toml)"
    echo "  │ qemu:   $(qemu-system-riscv64 --version | head -1)"
    echo "  ├──────────────────────────────────────────────────"
    echo "  │ Запуск с ГРАФИКОЙ:   Code/tools/run.sh   (одной командой, --help)"
    echo "  │ Сборка+запуск ядра:  cd Code && cargo run  (без экрана: -kernel/PVH)"
    echo "  │ Выход из QEMU:       Ctrl-A, затем X"
    echo "  └──────────────────────────────────────────────────"
    echo ""
  '';
}
