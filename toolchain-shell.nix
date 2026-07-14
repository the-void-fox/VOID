# Окружение сборки форка rust (vendor/rust) — тулчейн std-порта VOID (Веха 31).
#
# Использование (из корня репозитория):
#   nix-shell toolchain-shell.nix --run "cd vendor/rust && ./x.py build --stage 1 library \
#       --target riscv64gc-unknown-void-elf,x86_64-unknown-void"
#
# Стратегия (см. vendor/rust/bootstrap.toml):
#   - stage0 = наш же rustup stable 1.97.0 (никаких скачиваний бет);
#   - LLVM — внешний из nixpkgs (llvm-config), сабмодуль llvm-project НЕ нужен;
#   - rust-lld в sysroot stage1 подкладывается симлинком из rustup (см. заметку).
let
  pkgs = import <nixpkgs> { };
  llvm = pkgs.llvmPackages_latest;
in
pkgs.mkShell {
  name = "void-toolchain";

  packages = [
    llvm.llvm.dev # llvm-config
    llvm.llvm
    pkgs.python3
    pkgs.pkg-config
  ];

  # Библиотеки, которые тянет llvm-config --system-libs при линковке rustc_llvm.
  buildInputs = [ pkgs.zlib pkgs.zstd pkgs.libxml2 pkgs.ncurses ];

  shellHook = ''
    echo "  void-toolchain: LLVM $(llvm-config --version) · stage0 = $(rustc --version 2>/dev/null)"
  '';
}
