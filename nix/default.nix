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
