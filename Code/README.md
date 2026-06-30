# VOID — Code

Cargo workspace микроядра VOID. Концепция, ADR и роадмап — в `../Obsidian/10-projects/void/`.

## Структура
```
Code/
├── Cargo.toml            # workspace
├── rust-toolchain.toml   # пин Rust + target riscv64gc-unknown-none-elf (rustup)
├── .cargo/config.toml    # target по умолчанию + runner (QEMU)
├── kernel/               # микроядро (bare-metal, S-mode)
│   ├── linker.ld         # раскладка памяти (load 0x80200000)
│   ├── build.rs          # передаёт linker.ld линкеру
│   └── src/{main,uart}.rs, entry.s
└── libs/
    └── void-abi/         # общие типы границы ядро/userspace (ContentId, Cap)
```

## Требования
- Окружение из `../shell.nix` (QEMU, gdb) — `nix-shell` в корне репозитория.
- `rustup` на хосте (Rust-тулчейн и RISC-V target ставятся по `rust-toolchain.toml`).

## Сборка и запуск
```sh
nix-shell          # из корня репо: даёт qemu, gdb
cd Code
cargo run          # собрать ядро и загрузить в QEMU
```
Ожидаемый вывод — баннер «VOID — Веха 1» с hart id и адресом DTB.
Выход из QEMU: **Ctrl-A**, затем **X**.

## Отладка (позже)
QEMU с gdbstub: `qemu-system-riscv64 ... -s -S` + `gdb target/.../void-kernel`.
