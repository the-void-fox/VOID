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
│       │   └── x86_64/   # PVH+multiboot2, GDT/TSS, IDT, LAPIC/IOAPIC, PCI+MSI-X, PML4,
│       │                 #   VGA-текст+CP866+ANSI, PS/2-клавиатура (реальное железо)
│       ├── object.rs     # контент-адресуемый store: put/get, корни, GC, A/B-коммит
│       ├── cap.rs        # capability: домены, минт/grant/аттенуация/отзыв, .cspace
│       ├── proc.rs       # процессы, syscall'ы, IPC (CALL/RECV/REPLY), ленивые кучи
│       ├── linux.rs      # linux-abi (Веха 38): трансля́тор Linux-syscall'ов (бэкенд B)
│       ├── init.rs       # декларативный init (Веха 40): система из конфига-объекта store
│       ├── virtio_blk.rs # драйвер диска: virtqueue общий, транспорт mmio/pci — от арха
│       ├── virtio_net.rs # драйвер сети: две очереди RX/TX, опрос, сырые кадры наверх
│       └── ...           # sched, timer, executor, heap, frame, elf, checkpoint
├── programs/
│   ├── user/             # userspace: либа шимов (ecall / int 0x80) + 17 бинарей
│   │                     #   (vsh, posixfs, net-srv, threads, mini-sh, hello, драйверы…)
│   ├── std-hello/        # std-программы (Веха 31/35): тулчейн void, обычный cargo
│   ├── std-threads/      #   (std::thread + Arc<Mutex> + thread_local)
│   ├── wasi-run/         # WASI-раннер (Веха 39): wasmi поверх std-порта — бэкенд C
│   └── void-libc/        # C-глю (Веха 36): crt0 + стабы newlib поверх ABI VOID + specs
│                         #   (собирает ../nix/default.nix кросс-gcc'ом из pkgsCross)
├── libs/
│   ├── void-abi/         # общие типы границы ядро/userspace (ContentId, Cap, Rights)
│   └── void-store/       # формат и логика store (no_std, трейт BlockIo) —
│                         #   одна реализация на ядро и хост-утилиты
└── tools/
    └── void-store-import/ # мост host→store (отдельный крейт, хостовый musl-таргет)
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

## Импорт с хоста (Веха 29)
```sh
# сборка утилиты (в nix-shell: build-скриптам нужен cc; бинарь — статический musl)
nix-shell --run "cd Code/tools/void-store-import && cargo build --release"
T=tools/void-store-import/target/x86_64-unknown-linux-musl/release/void-store-import

$T void-disk.img ls                      # корни, поколение, объекты
$T void-disk.img put файл имя-корня      # файл → объект + корень
nix-store --dump ПУТЬ > a.nar            # любой путь, в т.ч. $(nix-build ...)
$T void-disk.img nar a.nar pkg/имя       # NAR → корень на каждый файл
```
Не запускать, пока образ занят QEMU (совместного доступа у store нет).

## std-программы (Веха 31)
Тулчейн `void` — stage1 форка `../vendor/rust` (таргеты `riscv64gc-unknown-void-elf`,
`x86_64-unknown-void`, порт std поверх syscall'ов VOID). Сборка тулчейна (один раз,
~40 мин; повтор после правок std — ~1 мин) и программ — см. рецепт в
`Obsidian/10-projects/void/notes/std-port.md`. Коротко:
```sh
nix-shell ../toolchain-shell.nix --run "cd ../vendor/rust && python3 x.py build \
  --stage 1 library --target x86_64-unknown-linux-gnu,riscv64gc-unknown-void-elf,x86_64-unknown-void"
rustup toolchain link void ../vendor/rust/build/x86_64-unknown-linux-gnu/stage1
# rust-lld в sysroot — симлинк из rustup; пересоздать после каждого прогона x.py

nix-shell --run "cd Code/programs/std-hello && cargo build --release"   # тулчейн из rust-toolchain.toml
tools/.../void-store-import void-disk.img put программа bin/<arch>/имя   # доставка мостом
```
После пересборки std у std-программ обязателен `rm -rf target` (cargo не
отслеживает sysroot — полусвежий кэш даёт бессвязные E0463).

## uutils/coreutils (Веха 32)
Форк-сабмодуль `../vendor/coreutils` (ветка `void`): multicall-бинарь с
`cat,echo,wc,head,ls,cp,mv,rm`. Рецепт сборки (кросс, оба таргета, RUSTFLAGS
строго таргет-скоуп — глобальный ломает host-сборку proc-macro) — в
`Obsidian/10-projects/void/notes/uutils.md`. Доставка мостом:
`put <elf> bin/<arch>/coreutils`, запуск: `run bin/coreutils ls`.

## Сеть (Веха 34)
Раннеры в `.cargo/config.toml` уже поднимают virtio-net на QEMU SLIRP
(`-netdev user`, гость `10.0.2.15`, шлюз/DNS `10.0.2.2`/`.3`). Ядро отдаёт лишь
сырые кадры (`virtio_net.rs`); стек ARP/IPv4/ICMP echo — в userspace-сервере
`bin/net-srv`, поднимается на загрузке и сам пингует шлюз. Из vsh:
```
vsh> ping 10.0.2.2      # RTT в мкс; SLIRP отвечает, не выходя из QEMU
```
Подробности — `Obsidian/10-projects/void/notes/virtio-net.md`.

## Потоки (Веха 35)
Ядро планирует НИТИ внутри процесса (общее адресное пространство и домен, свой
стек/TLS): `SYS_THREAD_SPAWN/EXIT/JOIN`, `SYS_FUTEX`, `SYS_SET_TLS`. Порт std
(`vendor/rust`) дорос до `std::thread`, futex `Mutex/Condvar` и нативного
`thread_local!` (TLS Variant I на riscv / II на x86). Демо:
```
vsh> run bin/threads       # no_std: 4 нити, счётчик 200000 под futex-мьютексом
vsh> run bin/threads-std   # std::thread + Arc<Mutex> + thread_local (доставить мостом)
```
`threads-std` — крейт `programs/std-threads` (тулчейн `void`, как std-hello);
доставка: `put <elf> bin/<arch>/threads-std`. Подробности —
`Obsidian/10-projects/void/notes/threads.md`.

## C-мир: nixpkgs-cross (Веха 36)
Настоящие C-программы по рецептам nixpkgs: кросс-gcc+newlib из
`pkgsCross.{riscv64,x86_64}-embedded` (бинарный кэш) + глю `programs/void-libc`
(crt0 + стабы newlib поверх ABI VOID: файлы — IPC к posixfs, sbrk — ленивый
SYS_MAP) + specs-файл. Сборка на хосте, из корня репо:
```sh
nix-build nix -A riscv64.hello && nix-build nix -A riscv64.bzip2   # и x86_64.*
tools/.../void-store-import void-disk.img put result/bin/hello bin/<arch>/hello-gnu
```
В vsh: `run bin/hello-gnu --greeting=Привет`, `run bin/bzip2 -z файл` /
`-d файл.bz2`. Ручная сборка своего C: `gcc -B<void-libc>/lib
-specs=<void-libc>/lib/void.specs prog.c` (тулчейн — `nix-build nix -A riscv64.cc`).
Подробности — `Obsidian/10-projects/void/notes/nixpkgs-cross.md`.

## Checkpoint процессов (Веха 37)
Вычисления переживают перезагрузку: процесс морозит себя `SYS_CHECKPOINT`
(образ — дерево объектов store под `proc/<arch>/<имя>`: страницы — дети
манифеста, дедуп/GC бесплатно), возврат 0 — живому, 1 — размороженному (setjmp).
```
vsh> run bin/freeze     # цикл, на шаге 5 — образ; итог 385 ✓
vsh> thaw пример        # хоть после перезагрузки: «продолжаю с шага 5, куча цела»
```
Подробности — `Obsidian/10-projects/void/notes/checkpoint.md`.

## linux-abi: бинари nixpkgs как есть (Веха 38, бэкенд B)
Неизменённые **static-PIE musl**-бинари из кэша nixpkgs работают под персоналией
Linux: тонкий транслятор Linux-syscall'ов в ядре (`kernel/src/linux.rs` +
`proc::linux_syscall`). `SYS_EXEC` авто-детектит `ET_DYN` → linux-личность
(`Proc.linux`); riscv `ecall` разводится флагом, x86 `syscall` ловится как #UD
(без MSR/стаба). `brk`/`mmap(anon)` — поверх ленивой кучи (Веха 22).
```sh
nix-shell -p pkgsCross.musl64.buildPackages.gcc --run \
  'x86_64-unknown-linux-musl-gcc -static-pie -fPIE hello.c -o hello'   # riscv64-musl для RISC-V
tools/.../void-store-import void-disk.img put hello bin/<arch>/lhello
```
В vsh: `run bin/lhello`, `run bin/busybox echo …` / `uname -a` / `seq 1 5`
(busybox 1.37 — один бинарь, мультиплекс по argv; файловые applet'ы отложены).
Подробности и рецепт сборки busybox — `Obsidian/10-projects/void/notes/linux-abi.md`.

## WASI: wasm-модули через wasmi (Веха 39, бэкенд C)
Неизменённые **wasm32-wasi**-модули работают через интерпретатор wasmi — тот же
.wasm на ОБЕИХ архитектурах. Микроядерно чисто: `programs/wasi-run` — обычная
std-программа (тулчейн `void`, крейт `wasmi`), ЯДРО НЕ ТРОНУТО. Читает .wasm как
файл (`std::fs` → posixfs), замыкает импорты `wasi_snapshot_preview1` на std
(fd_write → SYS_WRITE, proc_exit → SYS_EXIT, args → `std::env::args`).
```sh
cargo +stable build --release --target wasm32-wasip1     # любой wasi-модуль
tools/.../void-store-import void-disk.img put prog.wasm prog.wasm   # как файл персоналии
nix-shell shell.nix --run "cd programs/wasi-run && cargo build --release"  # тулчейн void
tools/.../void-store-import void-disk.img put …/wasi-run bin/<arch>/wasirun
```
В vsh: `run bin/wasirun hello.wasm [аргументы…]`. Так закрыты **все три бэкенда**
void-pkg (A: nixpkgs-cross · B: linux-abi · C: wasi). Подробности —
`Obsidian/10-projects/void/notes/wasi.md`.

## Декларативный init (Веха 40)
Система поднимается по КОНФИГУ-объекту из store (`kernel/src/init.rs`), а не по
зашитому в `kmain` сценарию. Конфиг — текст: `service posixfs store:rw` /
`shell vsh endpoint:posixfs store:xw env`; init минтит права по токенам в домены и
разводит эндпоинты по именам сервисов. Поколения = история корня, откат = смена
`system/current`:
```
vsh> switch gen2       # без сети; перезагрузка → ping «сети нет»
vsh> switch gen1       # полное; перезагрузка → сеть вернулась
```
Конфиг арх-нейтрален (init резолвит `bin/<arch>/*`) — один `system/*` на обе арх.
Пишется в vsh (`sysdef GEN FILE`) ИЛИ генерится настоящим Nix на хосте:
```sh
nix eval --raw --file nix/system.nix > /tmp/gen.conf     # язык Nix вычисляет конфиг
tools/.../void-store-import void-disk.img put /tmp/gen.conf system/gen3
```
Модель NixOS: язык вычисляет на хосте, VOID грузит результат. Подробности —
`Obsidian/10-projects/void/notes/declarative-init.md`.

## Реальное железо: загрузка на x86 (Веха 41)
x86-ядро грузится не только QEMU (PVH), но и настоящей прошивкой через **GRUB**
(multiboot2-заголовок в `entry.s`), рисует на **VGA** (0xB8000 — у ноутбука нет
COM-порта) и берёт размер RAM из **карты памяти** загрузчика (`platform_init` →
`arch::ram_limit()`). Собрать загрузочный USB и проверить (тот же путь, что на металле):
```sh
cargo build --release --target x86_64-unknown-none
nix-shell -p grub2 xorriso --run 'boot/mkboot.sh target/x86_64-unknown-none/release/void-kernel'
qemu-system-x86_64 -machine q35 -m 512M -cdrom boot/void.iso -nographic   # проверка через GRUB
sudo dd if=boot/void.iso of=/dev/sdX bs=4M status=progress && sync        # запись на флешку (СОТРЁТ!)
```
На реальной машине появятся баннер, карта памяти и сразу `vsh>` (QEMU-демо на металле
пропускаются — `arch::is_real_hardware()` по способу загрузки) — с **кириллицей** (шрифт
CP866 в знакогенераторе VGA, Веха 41) и **интерактивным вводом с PS/2-клавиатуры** (Веха 42,
`ps2.rs`: скан-коды 8042 → ASCII, IRQ1 через IOAPIC). Проверено на ASUS X54C. НЕ поедет пока:
диск/сеть (AHCI/реальный NIC вместо virtio), USB-клавиатура (если нет PS/2-эмуляции), русская
раскладка. Подробности — `Obsidian/10-projects/void/notes/platform.md`.

## Консоль: цвет и папки (Вехи 43–44)
vsh печатает **ANSI-коды** — их толкует и терминал QEMU (serial), и VGA-ядро (`vga.rs`
разбирает `ESC[…m`/`ESC[2J`, двигает аппаратный курсор): цвет и `clear` работают одинаково
в эмуляторе и на реальном экране (Веха 43, `Obsidian/…/notes/tty.md`). Persona **posixfs
получила иерархию** (Веха 44, `Obsidian/…/notes/folders.md`) — БЕЗ правок ядра: файл — корень
`f<путь>`, каталог — индекс-корень `d<путь>`. В vsh:
```
vsh/> mkdir notes && cd notes && echo привет > a.txt && ls && cat a.txt && pwd
```
Команды `cd`/`pwd`/`mkdir`/`rm` + путь-осознанные `ls [DIR]`/`cat`/`tail`/`echo`/`mv`;
приглашение показывает текущий каталог. Открытый техдолг — `Obsidian/…/notes/known-gaps.md`.

## Отладка
QEMU с gdbstub: добавить `-s -S` к команде раннера, затем
`gdb target/<таргет>/debug/void-kernel` → `target remote :1234`.
