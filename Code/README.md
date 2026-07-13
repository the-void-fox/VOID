# VOID — Code

Cargo workspace микроядра VOID. Обзор системы — в [корневом README](../README.md);
концепция, ADR и роадмап — в `../Obsidian/10-projects/void/`.

## Структура
```
Code/
├── Cargo.toml            # workspace (kernel + libs; programs/user — отдельный крейт)
├── rust-toolchain.toml   # Rust + таргеты riscv64gc-unknown-none-elf, x86_64-unknown-none
├── .cargo/config.toml    # таргет по умолчанию (riscv) + QEMU-раннеры обеих архитектур
├── kernel/
│   ├── linker.ld         # раскладка riscv (load 0x80200000)
│   ├── linker-x86_64.ld  # раскладка x86 (load 0x100000, PVH-нота в PT_NOTE)
│   ├── build.rs          # линкер-скрипт по архе + сборка programs/user под арху ядра
│   └── src/
│       ├── arch/         # контракт архитектур (ADR 0005)
│       │   ├── riscv64/  # SBI, PLIC, Sv39, trap/context asm
│       │   └── x86_64/   # PVH-трамплин, GDT/TSS, IDT, LAPIC/IOAPIC, PCI+MSI-X, PML4
│       ├── object.rs     # контент-адресуемый store: put/get, корни, GC, A/B-коммит
│       ├── cap.rs        # capability: домены, минт/grant/аттенуация/отзыв, .cspace
│       ├── proc.rs       # процессы, syscall'ы, IPC (CALL/RECV/REPLY), ленивые кучи
│       ├── virtio_blk.rs # драйвер: virtqueue общий, транспорт mmio/pci — от арха
│       └── ...           # sched, timer, executor, heap, frame, elf
├── programs/
│   └── user/             # userspace: либа шимов (ecall / int 0x80) + 14 бинарей
│                         #   (vsh, posixfs, mini-sh, hello, bench, драйверы-демо…)
└── libs/
    └── void-abi/         # общие типы границы ядро/userspace (ContentId, Cap, Rights)
```

## Требования
- Окружение из `../shell.nix` (QEMU, gdb) — `nix-shell` **в корне репозитория**.
- `rustup` на хосте: тулчейн и таргеты ставятся по `rust-toolchain.toml`.

## Сборка и запуск
```sh
nix-shell                      # из корня репо
cd Code
truncate -s 16M void-disk.img  # один раз: ОБЩИЙ диск обеих архитектур (gitignore)

cargo run                                   # RISC-V: QEMU virt, OpenSBI → S-mode
cargo run --target x86_64-unknown-none      # x86_64: QEMU q35, PVH direct boot
```
Выход из QEMU: **Ctrl-A**, затем **X**. Состояние store на `void-disk.img`
переживает перезагрузки и разделяется обеими архитектурами (корни программ
разведены как `bin/<arch>/<имя>`).

## Отладка
QEMU с gdbstub: добавить `-s -S` к команде раннера, затем
`gdb target/<таргет>/debug/void-kernel` → `target remote :1234`.
