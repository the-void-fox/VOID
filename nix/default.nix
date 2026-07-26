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
