# Веха 36 — nixpkgs-cross: C-мир поверх void-libc (ADR 0004, бэкенд A, магистраль).
#
# nixpkgs здесь — «книга рецептов и сборочный цех НА ХОСТЕ», не программа внутри VOID:
# кросс-тулчейны pkgsCross (gcc + newlib для bare-metal триплетов riscv64-none-elf /
# x86_64-elf) приезжают из бинарного кэша готовыми, а весь порт — тонкое глю
# `Code/programs/void-libc` (crt0 + syscall-стабы newlib поверх ABI VOID) и specs-файл,
# после которого КАЖДЫЙ вызов кросс-gcc линкует настоящие VOID-исполняемые ELF.
# Раскладку задаёт общий с Rust-программами linker.ld (один источник правды).
#
# Сборка (на хосте, из корня репозитория):
#   nix-build nix -A riscv64.hello  && result/bin/hello  → ELF под VOID/riscv64
#   nix-build nix -A x86_64.bzip2                        → ELF под VOID/x86_64
#   nix-build nix -A riscv64.cc     → голый тулчейн с глю (gcc -B… -specs=void.specs)
# Доставка в образ — мостом (Веха 29): void-store-import put <elf> bin/<arch>/<имя>.
{ nixpkgs ? <nixpkgs> }:
let
  mkWorld = crossAttr: archName:
    let
      pkgs = (import nixpkgs { }).pkgsCross.${crossAttr};
      inherit (pkgs) stdenv;

      # Глю: crt0.o + libvoid.a (стабы) + линкер-скрипт + specs — «прошивка» gcc под VOID.
      # specs заменяет startfile (crt0 newlib → наш) и группирует -lvoid -lc -lgcc,
      # так что порядок разрешения символов верен при любом порядке объектов пакета.
      void-libc = stdenv.mkDerivation {
        pname = "void-libc";
        version = "0.36";
        src = ../Code/programs/void-libc;
        linkerScript = ../Code/programs/user/linker.ld;
        dontConfigure = true;
        buildPhase = ''
          $CC -O2 -fno-asynchronous-unwind-tables -c crt0.S -o crt0.o
          $CC -O2 -fno-asynchronous-unwind-tables -c void.c -o void.o
          # Веха 54 — lx_emul (C-порт Linux-API шима) в тот же libvoid.a: программы, не
          # зовущие его функций (hello/bzip2), .o не тянут (статический архив).
          $CC -O2 -fno-asynchronous-unwind-tables -c lx_emul.c -o lx_emul.o
          $AR rcs libvoid.a void.o lx_emul.o
        '';
        installPhase = ''
          mkdir -p $out/lib
          # имя void-crt0.o уникально: обёртка ставит -B newlib раньше нашего, и
          # crt0.o%s из specs нашёл бы ИХ crt0 (libgloss, ждёт __bss_start)
          cp crt0.o $out/lib/void-crt0.o
          cp libvoid.a void.specs void-decls.h $out/lib/
          # заголовок Linux-API шима — драйверам (чистый API, без syscall.h)
          cp lx_emul.h $out/lib/
          # syscall.h — прямой доступ к vsys_* (start_cap/mmio_map/exit); нужен портированному
          # e1000-драйверу (Веха 69), который читает caps сам, а не через lx_emul-обёртку
          cp syscall.h $out/lib/
          cp $linkerScript $out/lib/void.ld
        '';
      };

      # Флаги, делающие из кросс-gcc компилятор VOID: искать глю через -B, линковать
      # по specs. Едут через NIX_CFLAGS_COMPILE — cc-wrapper добавляет их к каждому
      # вызову компилятора, включая линковку и configure-пробы autotools.
      voidCFlags = "-B${void-libc}/lib -specs=${void-libc}/lib/void.specs";

      # Пакет nixpkgs → пакет VOID: тот же рецепт (src, фазы, патчи), наша линковка.
      # Тесты не гоняем (бинари не исполняются на хосте), shared не бывает (ET_EXEC).
      # Флаги — через env.*: при __structuredAttrs обычный атрибут в окружение сборки
      # не попадает, и cc-wrapper его не увидел бы.
      voidify = drv: drv.overrideAttrs (old: {
        env = (old.env or { }) // {
          NIX_CFLAGS_COMPILE =
            toString (old.env.NIX_CFLAGS_COMPILE or old.NIX_CFLAGS_COMPILE or "")
            + " " + voidCFlags;
        };
        doCheck = false;
        dontDisableStatic = true;
      });
    in rec {
      arch = archName;
      inherit void-libc voidify;
      cc = stdenv.cc;

      # GNU hello — символ «настоящий autotools-пакет собрался под VOID как есть».
      hello = voidify (pkgs.hello.overrideAttrs (old: {
        # gnulib видит функции глю линковкой, а декларации ждёт от stdlib.h
        # newlib, где их нет, — дописываем декларации в config.h (его каждый
        # gnulib-файл включает первым).
        postConfigure = (old.postConfigure or "") + ''
          cat ${void-libc}/lib/void-decls.h >> config.h
        '';
        # gnulib на неизвестной ОС перестраховывается заменами — nls нам не нужен;
        # getrlimit configure находит в libc (реентерабельный огрызок newlib), но
        # sys/resource.h его структур не даёт — глушим, gnulib возьмёт фолбэк.
        configureFlags = (old.configureFlags or [ ])
          ++ [
            "--disable-nls"
            # getdtablesize/getprogname в libvoid есть — иначе gnulib компилирует
            # замены: одна безусловно зовёт getrlimit (нет в заголовках newlib),
            # другая — #error «не портировано на эту ОС».
            "ac_cv_func_getdtablesize=yes"
            "gl_cv_func_getdtablesize_works=yes"
            "ac_cv_func_getprogname=yes"
            # fcntl-стаб глю: иначе gnulib-замена тянет модуль dupfd, которого
            # нет в minimal-наборе hello (ловится на x86, где newlib без fcntl).
            "ac_cv_func_fcntl=yes"
            "gl_cv_func_fcntl_f_dupfd_works=yes"
            "gl_cv_func_fcntl_f_dupfd_cloexec=yes"
          ];
      }));

      # lx_e1000 (Веха 54) — Intel e1000 как Linux-СТИЛЕВОЙ драйвер на C поверх шима lx_emul.
      # НЕ пакет nixpkgs, а свой C-исходник: компилируем кросс-gcc против void-libc (specs
      # линкуют VOID-ELF), заголовок шима — из глю (-I). Едет на диск мостом, init спавнит.
      lx_e1000 = stdenv.mkDerivation {
        pname = "lx_e1000-c";
        version = "0.54";
        src = ../Code/programs/lx-cdriver;
        dontConfigure = true;
        hardeningDisable = [ "all" ];
        buildPhase = ''
          $CC ${voidCFlags} -I${void-libc}/lib -O2 -static lx_e1000.c -o lx_e1000
        '';
        installPhase = ''
          mkdir -p $out/bin
          cp lx_e1000 $out/bin/
        '';
      };

      # lx_sort (Веха 55) — НЕИЗМЕНЁННЫЙ lib/sort.c ядра Linux (6.18.7) против рукописных
      # шим-заголовков linux/*.h (начало lx_emul-заголовков): первый реальный .c из Linux на VOID.
      # -I. находит linux/ (sort.c + harness), -DCONFIG_64BIT (обе арх 64-битные) → 64-битный swap.
      lx_sort = stdenv.mkDerivation {
        pname = "lx-sort";
        version = "0.55";
        src = ../Code/programs/lx-linux;
        dontConfigure = true;
        hardeningDisable = [ "all" ];
        buildPhase = ''
          $CC ${voidCFlags} -I. -DCONFIG_64BIT -O2 -static main.c linux-src/sort.c -o lx-sort
        '';
        installPhase = ''
          mkdir -p $out/bin
          cp lx-sort $out/bin/
        '';
      };

      # lx_argv (Веха 56) — НЕИЗМЕНЁННЫЙ lib/argv_split.c ядра Linux (6.18.7) против рукописных
      # шим-заголовков linux/*.h, теперь ВКЛЮЧАЯ аллокатор: kmalloc-семейство даёт Lx_kit-рантайм
      # (lx_kit.c) поверх кучи newlib. Первый портированный .c, ЧЕСТНО аллоцирующий память ядровым
      # kmalloc/kstrndup — фундамент kit под драйверы. -I. находит linux/ (наши шимы + vendored .c).
      lx_argv = stdenv.mkDerivation {
        pname = "lx-argv";
        version = "0.56";
        src = ../Code/programs/lx-linux;
        dontConfigure = true;
        hardeningDisable = [ "all" ];
        buildPhase = ''
          $CC ${voidCFlags} -I. -DCONFIG_64BIT -O2 -static main_argv.c linux-src/argv_split.c lx_kit.c -o lx-argv
        '';
        installPhase = ''
          mkdir -p $out/bin
          cp lx-argv $out/bin/
        '';
      };

      # lx_list (Веха 57) — НЕИЗМЕНЁННЫЙ lib/list_sort.c ядра Linux (6.18.7): устойчивая
      # merge-сортировка двусвязного списка. Растим шим до linux/list.h (костяк ядра) + compiler.h;
      # container_of вынесен в свой заголовок. Харнесс строит список нашим list.h и зовёт реальный
      # list_sort(). Доказывает настоящий linux/list.h. lx_kit.c не нужен (список — без аллокатора).
      lx_list = stdenv.mkDerivation {
        pname = "lx-list";
        version = "0.57";
        src = ../Code/programs/lx-linux;
        dontConfigure = true;
        hardeningDisable = [ "all" ];
        buildPhase = ''
          $CC ${voidCFlags} -I. -DCONFIG_64BIT -O2 -static main_list.c linux-src/list_sort.c -o lx-list
        '';
        installPhase = ''
          mkdir -p $out/bin
          cp lx-list $out/bin/
        '';
      };

      # lx_bits (Веха 58) — НЕИЗМЕНЁННЫЙ lib/hweight.c ядра Linux (6.18.7, софтовый popcount) +
      # наш шим linux/bitops.h (set/clear/test_bit, BIT/GENMASK, for_each_set_bit, hweight-маршрут в
      # hweight.c) и asm/types.h. Харнесс гоняет битовые операции; -I. находит linux/ и asm/.
      # lx_kit.c нужен ради printk.
      lx_bits = stdenv.mkDerivation {
        pname = "lx-bits";
        version = "0.58";
        src = ../Code/programs/lx-linux;
        dontConfigure = true;
        hardeningDisable = [ "all" ];
        buildPhase = ''
          $CC ${voidCFlags} -I. -DCONFIG_64BIT -O2 -static main_bits.c linux-src/hweight.c lx_kit.c -o lx-bits
        '';
        installPhase = ''
          mkdir -p $out/bin
          cp lx-bits $out/bin/
        '';
      };

      # lx_err (Веха 59) — наш шим linux/err.h (идиома «ошибка в указателе»: ERR_PTR/PTR_ERR/
      # IS_ERR/…) + linux/errno.h (проход к newlib). Инфраструктурный заголовок — проверяется
      # харнессом (в бою раскроется в драйвере). lx_kit.c — ради printk.
      lx_err = stdenv.mkDerivation {
        pname = "lx-err";
        version = "0.59";
        src = ../Code/programs/lx-linux;
        dontConfigure = true;
        hardeningDisable = [ "all" ];
        buildPhase = ''
          $CC ${voidCFlags} -I. -DCONFIG_64BIT -O2 -static main_err.c lx_kit.c -o lx-err
        '';
        installPhase = ''
          mkdir -p $out/bin
          cp lx-err $out/bin/
        '';
      };

      # lx_io (Веха 60) — наш шим linux/io.h (MMIO-аксессоры readl/writel/… + ioremap identity).
      # Харнесс трактует буфер как блок регистров и проверяет round-trip всех ширин + LE-раскладку.
      # Настоящее окно регистров даёт lx_emul по MMIO-cap — сведём позже. lx_kit.c — ради printk.
      lx_io = stdenv.mkDerivation {
        pname = "lx-io";
        version = "0.60";
        src = ../Code/programs/lx-linux;
        dontConfigure = true;
        hardeningDisable = [ "all" ];
        buildPhase = ''
          $CC ${voidCFlags} -I. -DCONFIG_64BIT -O2 -static main_io.c lx_kit.c -o lx-io
        '';
        installPhase = ''
          mkdir -p $out/bin
          cp lx-io $out/bin/
        '';
      };

      # lx_delay (Веха 61) — наш шим linux/delay.h (udelay/mdelay/ndelay), тела в Lx_kit: буси-
      # ожидание по монотонному времени VOID (gettimeofday → vsys_ticks). Харнесс замеряет, что
      # пауза РЕАЛЬНО прошла (>= запрошенного). lx_kit.c даёт тела задержек + printk.
      lx_delay = stdenv.mkDerivation {
        pname = "lx-delay";
        version = "0.61";
        src = ../Code/programs/lx-linux;
        dontConfigure = true;
        hardeningDisable = [ "all" ];
        buildPhase = ''
          $CC ${voidCFlags} -I. -DCONFIG_64BIT -O2 -static main_delay.c lx_kit.c -o lx-delay
        '';
        installPhase = ''
          mkdir -p $out/bin
          cp lx-delay $out/bin/
        '';
      };

      # lx_sched (Веха 62) — костяк кооперативного планировщика Lx_kit: задача = отдельный стек +
      # setjmp/longjmp, один поток (модель Genode dde_linux). Тела — в lx_kit.c (lx_sched.h).
      # Харнесс: Демо A round-robin по yield, Демо B ping/pong по block/unblock (wait_event/wake_up),
      # печать адреса локали доказывает отдельные стеки. Обе арх.
      lx_sched = stdenv.mkDerivation {
        pname = "lx-sched";
        version = "0.62";
        src = ../Code/programs/lx-linux;
        dontConfigure = true;
        hardeningDisable = [ "all" ];
        buildPhase = ''
          $CC ${voidCFlags} -I. -DCONFIG_64BIT -O2 -static main_sched.c lx_kit.c -o lx-sched
        '';
        installPhase = ''
          mkdir -p $out/bin
          cp lx-sched $out/bin/
        '';
      };

      # lx_timer (Веха 63) — jiffies (linux/jiffies.h) + таймеры (linux/timer.h) поверх планировщика.
      # Тела в lx_kit.c: jiffies по монотонному времени VOID, очередь таймеров, idle-путь стреляет;
      # msleep стал УСТУПАЮЩИМ. Харнесс: две сони чередуются (msleep уступает) + таймеры стреляют по
      # возрастанию expires (2 3 1). Обе арх.
      lx_timer = stdenv.mkDerivation {
        pname = "lx-timer";
        version = "0.63";
        src = ../Code/programs/lx-linux;
        dontConfigure = true;
        hardeningDisable = [ "all" ];
        buildPhase = ''
          $CC ${voidCFlags} -I. -DCONFIG_64BIT -O2 -static main_timer.c lx_kit.c -o lx-timer
        '';
        installPhase = ''
          mkdir -p $out/bin
          cp lx-timer $out/bin/
        '';
      };

      # lx_wait (Веха 64) — очереди ожидания (linux/wait.h) + completion (linux/completion.h) поверх
      # block/unblock: wait_event/wake_up, wait_event_timeout (через таймер Вехи 63), wait_for_completion.
      # Тела __lx_wait/__lx_wake_up — в lx_kit.c; completion — inline в шиме. Харнесс: producer/consumer,
      # completion, таймаут (истечение → 0, пробуждение → остаток jiffies). Обе арх.
      lx_wait = stdenv.mkDerivation {
        pname = "lx-wait";
        version = "0.64";
        src = ../Code/programs/lx-linux;
        dontConfigure = true;
        hardeningDisable = [ "all" ];
        buildPhase = ''
          $CC ${voidCFlags} -I. -DCONFIG_64BIT -O2 -static main_wait.c lx_kit.c -o lx-wait
        '';
        installPhase = ''
          mkdir -p $out/bin
          cp lx-wait $out/bin/
        '';
      };

      # lx_work (Веха 65) — рабочие очереди (linux/workqueue.h): schedule_work/schedule_delayed_work/
      # flush_*/cancel_* поверх задачи-воркера (Веха 62) и таймеров (Веха 63). У e1000 6.18 watchdog —
      # на delayed_work. Тела в lx_kit.c. Харнесс: FIFO-работы, delayed через ~30 мс, cancel до срабатывания.
      lx_work = stdenv.mkDerivation {
        pname = "lx-work";
        version = "0.65";
        src = ../Code/programs/lx-linux;
        dontConfigure = true;
        hardeningDisable = [ "all" ];
        buildPhase = ''
          $CC ${voidCFlags} -I. -DCONFIG_64BIT -O2 -static main_work.c lx_kit.c -o lx-work
        '';
        installPhase = ''
          mkdir -p $out/bin
          cp lx-work $out/bin/
        '';
      };

      # lx_driver (Веха 66) — driver-model (linux/device.h) + module_init-редирект (linux/module.h):
      # регистрация связывает устройство с драйвером по правилу шины и зовёт .probe (упрощённый
      # drivers/base/dd.c). На этом встанет PCI. Тела в lx_kit.c. Харнесс: фейк-шина/драйвер/устройство
      # проходят module_init→register→match→probe→remove. Обе арх.
      lx_driver = stdenv.mkDerivation {
        pname = "lx-driver";
        version = "0.66";
        src = ../Code/programs/lx-linux;
        dontConfigure = true;
        hardeningDisable = [ "all" ];
        buildPhase = ''
          $CC ${voidCFlags} -I. -DCONFIG_64BIT -O2 -static main_driver.c lx_kit.c -o lx-driver
        '';
        installPhase = ''
          mkdir -p $out/bin
          cp lx-driver $out/bin/
        '';
      };

      # lx_pci (Веха 67) — слой PCI (linux/pci.h) поверх driver-model: pci_dev встраивает device,
      # pci_driver — device_driver, pci_register_driver крутит ту же связку register→match→probe,
      # но match по id_table (vendor/device); конфиг-пространство и BAR'ы в pci_dev. Тела в lx_kit.c.
      # Харнесс: синтетический 8086:100E (e1000) → match → probe читает BAR и конфиг. Обе арх.
      lx_pci = stdenv.mkDerivation {
        pname = "lx-pci";
        version = "0.67";
        src = ../Code/programs/lx-linux;
        dontConfigure = true;
        hardeningDisable = [ "all" ];
        buildPhase = ''
          $CC ${voidCFlags} -I. -DCONFIG_64BIT -O2 -static main_pci.c lx_kit.c -o lx-pci
        '';
        installPhase = ''
          mkdir -p $out/bin
          cp lx-pci $out/bin/
        '';
      };

      # lx_e1000_port (Веха 68) — НЕИЗМЕНЁННЫЙ драйвер Intel e1000 (Linux 6.18.7:
      # drivers/net/ethernet/intel/e1000/{e1000_hw,e1000_main,e1000_param}.c, GPL-2.0) собран против
      # наших шимов linux/*.h + рантайма Lx_kit (lx_kit.c) + сетевых заглушек (lx_net.c). Vendored-код
      # компилируется с ядровыми флагами (-Wno-unused-parameter/-Wno-pointer-sign). Харнесс зовёт
      # ЧИСТУЮ логику e1000_hw.c (set_mac_type/set_media_type). Обе арх. Полный probe/TX/RX — след. веха.
      lx_e1000_port = stdenv.mkDerivation {
        pname = "lx-e1000";
        version = "0.68";
        src = ../Code/programs/lx-linux;
        dontConfigure = true;
        hardeningDisable = [ "all" ];
        buildPhase = ''
          $CC ${voidCFlags} -I. -Ilinux-src/e1000 -DCONFIG_64BIT \
            -Wno-unused-parameter -Wno-pointer-sign -O2 -static \
            main_e1000.c linux-src/e1000/e1000_main.c linux-src/e1000/e1000_hw.c \
            linux-src/e1000/e1000_param.c lx_kit.c lx_net.c -o lx-e1000
        '';
        installPhase = ''
          mkdir -p $out/bin
          cp lx-e1000 $out/bin/
        '';
      };

      # lx_e1000_drv (Вехи 69–73) — тот же НЕИЗМЕНЁННЫЙ e1000, но спавнится init'ом как userspace-драйвер
      # и ходит к НАСТОЯЩЕМУ QEMU-e1000: main() маппит BAR0 по MMIO-cap (SYS_MMIO_MAP, start_cap 0),
      # кладёт окно в hw->hw_addr, а vendored e1000_hw.c через er32/ew32 сбрасывает карту и читает
      # MAC/скорость с реального железа (69); TX по DMA (70), RX + ARP round-trip (71); RX по ПРЕРЫВАНИЮ
      # (72): request_irq↔vsys_irq_wait (IRQ-cap, start_cap 2) — планировщик Lx_kit спит на прерывании;
      # PING (73): ARP узнаёт MAC шлюза → ICMP echo request → приём echo reply по прерыванию.
      # Bring-up — как задача Lx_kit (msleep уступает). syscall.h из void-libc даёт vsys_*. e1000 в QEMU —
      # только x86; на riscv собирается/импортируется, не спавнится.
      lx_e1000_drv = stdenv.mkDerivation {
        pname = "lx-e1000-hw";
        version = "0.73";
        src = ../Code/programs/lx-linux;
        dontConfigure = true;
        hardeningDisable = [ "all" ];
        buildPhase = ''
          $CC ${voidCFlags} -I. -Ilinux-src/e1000 -I${void-libc}/lib -DCONFIG_64BIT -DLX_HAVE_SYSCALL \
            -Wno-unused-parameter -Wno-pointer-sign -O2 -static \
            drv_e1000.c linux-src/e1000/e1000_main.c linux-src/e1000/e1000_hw.c \
            linux-src/e1000/e1000_param.c lx_kit.c lx_net.c -o lx-e1000-hw
        '';
        installPhase = ''
          mkdir -p $out/bin
          cp lx-e1000-hw $out/bin/
        '';
      };

      # bzip2 — простой Makefile и честная утилита: сжатие файлов прямо в vsh.
      # Собираем только статический CLI (shared-библиотеке в мире ET_EXEC делать нечего).
      bzip2 = voidify (pkgs.bzip2.overrideAttrs (old: {
        outputs = [ "out" ]; # рецепту хватает одного: только CLI, без dev/man
        makeFlags = (old.makeFlags or [ ]) ++ [ "bzip2" ];
        # заголовки newlib не декларируют lstat/utime (функции даёт глю) —
        # дописываем декларации сразу после include-блока BZ_UNIX
        postPatch = ''
          substituteInPlace bzip2.c --replace-fail '#   include <utime.h>' \
            '#   include <utime.h>
          extern int lstat ( const char *, struct stat * );
          extern int utime ( const char *, const void * );'
        '';
        installPhase = ''
          mkdir -p $out/bin
          cp bzip2 $out/bin/
        '';
      }));
    };
in {
  riscv64 = mkWorld "riscv64-embedded" "riscv64";
  x86_64 = mkWorld "x86_64-embedded" "x86_64";
}
