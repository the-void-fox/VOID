//! vvsh-core — маленький гомоиконный Lisp VOID (ADR 0006): reader + значения + вычислитель +
//! нормализатор системного конфига. `no_std`+`alloc`, без зависимостей.
//!
//! Конвейер: текст `.vv` → парсинг → вычисление НА VOID → нормализованный конфиг (те же строки
//! `service …`/`shell …`, что сегодня даёт `nix/system.nix`). Потребитель — программа
//! `bin/<arch>/vvsh` (A2); ядро крейт не линкует. Host-тестируем (`no_std` только вне `test`) —
//! парсер/eval/import гоняем `cargo test` без QEMU.
//!
//! - M1a: [`build_config`] — один файл, без импортов.
//! - M1b: [`build_config_with`] + [`ModuleLoader`] — `(import "модуль.vv")` со слиянием (`append`).
//!
//! Раскладка «FS-источник + поколение-сборка» и вехи — в [[vvsh-config-layout]].

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod config;
pub mod eval;
pub mod reader;
pub mod value;
pub mod words;

pub use config::normalize_config;
pub use eval::{eval_program, root_env, Interp, ModuleLoader, NoLoader};
pub use reader::{read_all, ReadError};
pub use value::{Env, EvalError, Value};
pub use words::split_words;

use alloc::string::String;

/// Полный конвейер с загрузчиком модулей: текст `.vv` → нормализованный конфиг (или текст ошибки).
/// `import` внутри резолвится через `loader` (бинарь — posixfs, тесты — карта в памяти).
pub fn build_config_with(src: &str, loader: &dyn ModuleLoader) -> Result<String, String> {
    let interp = Interp::new(loader);
    let forms = read_all(src).map_err(|e| e.0)?;
    let val = interp.eval_program(&forms).map_err(|e| e.0)?;
    normalize_config(&val).map_err(|e| e.0)
}

/// Как [`build_config_with`], но без загрузчика: любой `import` — ошибка. Для конфига из одного файла.
pub fn build_config(src: &str) -> Result<String, String> {
    build_config_with(src, &NoLoader)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::cell::Cell;

    fn eval_str(src: &str) -> Value {
        let forms = read_all(src).expect("парсинг");
        eval_program(&forms).expect("вычисление")
    }

    /// Веха 160 — РАСКЛАДКА ПАНЕЛИ доезжает до конфига: `bar.vv` пишется списками, а на выходе
    /// обязаны быть строки `bar группа остров` в том же порядке. Проверка на хосте, потому что
    /// на живой системе «панель выглядит как раньше» означает и «конфиг применился», и «конфиг
    /// не применился, сработало умолчание» — эти два случая снаружи не различить.
    #[test]
    fn bar_layout_reaches_config() {
        let src = "\
left = [\"clock\", \"metrics\"]
center = [\"title\"]
right = [\"gen\"]
system(
  append(
    map(|i| bar(\"left\", i), left),
    map(|i| bar(\"center\", i), center),
    map(|i| bar(\"right\", i), right),
  )
)";
        assert_eq!(
            build_config(src).expect("конфиг"),
            "bar left clock\nbar left metrics\nbar center title\nbar right gen\n"
        );
    }

    /// Пустая группа — законный выбор, а не ошибка: панель без часов это по-прежнему панель.
    #[test]
    fn bar_layout_may_be_empty() {
        let src = "system(map(|i| bar(\"left\", i), []))";
        assert_eq!(build_config(src).expect("конфиг"), "");
    }

    /// Веха 167 — ПРОГРАММЫ ПО УМОЛЧАНИЮ доезжают до конфига целой строкой, а число полей у
    /// них проверяет тот же словарь: `(default "terminal")` — опечатка, и падать она обязана на
    /// `rebuild`, а не молчать в загруженной системе.
    #[test]
    fn default_apps_reach_config() {
        let src = "system([default(\"terminal\", \"term\"), default(\"files\", \"fm\")])";
        assert_eq!(
            build_config(src).expect("конфиг"),
            "default terminal term\ndefault files fm\n"
        );
        assert!(build_config("system([default(\"terminal\")])").is_err());
        assert!(build_config("system([default(\"a\", \"b\", \"c\")])").is_err());
    }

    /// Веха 172 — АВТОЗАПУСК доезжает до конфига одной строкой со списком имён, и он же
    /// принимает список одним аргументом: в модуле список лежит переменной (`open`), и заставлять
    /// человека раскрывать её руками значило бы протащить наружу устройство сборки.
    ///
    /// Пустой список — ошибка НАМЕРЕННО: «ничего не открывать» пишется отсутствием записи, а
    /// модуль `autostart.vv` для этого и проверяет `null?(open)`. Иначе в поколении оказалась бы
    /// строка `autostart` без единого имени — то есть запись, не значащая ничего.
    #[test]
    fn autostart_reaches_config() {
        assert_eq!(
            build_config("system([autostart(\"welcome\", \"term\")])").expect("конфиг"),
            "autostart welcome term\n"
        );
        assert_eq!(
            build_config("system([autostart([\"welcome\"])])").expect("конфиг"),
            "autostart welcome\n"
        );
        assert!(build_config("system([autostart()])").is_err());
    }

    /// Число полей у `bar` проверяет СЛОВАРЬ видов: опечатка обязана падать на `rebuild`,
    /// а не пропадать молча в загруженной системе.
    #[test]
    fn bar_wants_two_fields() {
        assert!(build_config("system([bar(\"left\")])").is_err());
        assert!(build_config("system([bar(\"left\", \"clock\", \"lang\")])").is_err());
    }

    #[test]
    fn arithmetic() {
        assert_eq!(eval_str("1 + 2 + 3"), Value::Int(6));
        assert_eq!(eval_str("10 - 3 - 2"), Value::Int(5));
        assert_eq!(eval_str("-(5)"), Value::Int(-5));
        assert_eq!(eval_str("2 * 3 * 4"), Value::Int(24));
        assert_eq!(eval_str("2 * 3 + (10 - 4)"), Value::Int(12));
    }

    #[test]
    fn let_if_cond() {
        assert_eq!(eval_str("let([[x, 2], [y, 3]], x + y)"), Value::Int(5));
        assert_eq!(eval_str("if true { 1 } else { 2 }"), Value::Int(1));
        assert_eq!(eval_str("if false { 1 } else { 2 }"), Value::Int(2));
        assert_eq!(eval_str("cond([false, 1], [true, 2], [else, 3])"), Value::Int(2));
        assert_eq!(eval_str("cond([false, 1], [else, 3])"), Value::Int(3));
    }

    #[test]
    fn closures_and_define() {
        assert_eq!(eval_str("sq = |x| x * x\n sq(7)"), Value::Int(49));
        assert_eq!(
            eval_str("add = |a, b| a + b\n add(4, 5)"),
            Value::Int(9)
        );
        assert_eq!(
            eval_str("adder = |n| |x| x + n\n inc = adder(1)\n inc(41)"),
            Value::Int(42)
        );
    }

    #[test]
    fn list_ops() {
        assert_eq!(
            eval_str("append([1, 2], [3])"),
            Value::list(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
        );
        assert_eq!(eval_str("car([1, 2, 3])"), Value::Int(1));
        assert_eq!(
            eval_str("cdr([1, 2, 3])"),
            Value::list(vec![Value::Int(2), Value::Int(3)])
        );
        assert_eq!(eval_str("null?([])"), Value::Bool(true));
        assert_eq!(eval_str("null?([1])"), Value::Bool(false));
        assert_eq!(eval_str("2 == 1 + 1"), Value::Bool(true));
    }

    #[test]
    fn length_and_pipe() {
        assert_eq!(eval_str("length([1, 2, 3])"), Value::Int(3));
        assert_eq!(eval_str("count([])"), Value::Int(0));
        // конвейер (thread-last): значение течёт последним аргументом
        assert_eq!(eval_str("[1, 2, 3] |> length"), Value::Int(3));
        assert_eq!(eval_str("[1, 2] |> cons(0) |> count"), Value::Int(3));
        assert_eq!(
            eval_str("[1, 2] |> append([9])"),
            Value::list(vec![Value::Int(9), Value::Int(1), Value::Int(2)])
        );
    }

    #[test]
    fn map_and_filter() {
        assert_eq!(
            eval_str("map(|x| x * x, [1, 2, 3])"),
            Value::list(vec![Value::Int(1), Value::Int(4), Value::Int(9)])
        );
        assert_eq!(
            eval_str("filter(|x| x == 2, [1, 2, 3, 2])"),
            Value::list(vec![Value::Int(2), Value::Int(2)])
        );
        // спец-формы map/filter текут в конвейере (через (quote acc))
        assert_eq!(
            eval_str("[1, 2, 3] |> map(|x| x * x)"),
            Value::list(vec![Value::Int(1), Value::Int(4), Value::Int(9)])
        );
        assert_eq!(
            eval_str("[1, 2, 3, 4] |> filter(|x| x == 2) |> count"),
            Value::Int(1)
        );
    }

    #[test]
    fn quote_and_atoms() {
        assert_eq!(eval_str("quote(foo)"), Value::sym("foo"));
        assert_eq!(eval_str("\"hi\\nthere\""), Value::str("hi\nthere"));
        assert_eq!(eval_str("# коммент\n42"), Value::Int(42));
    }

    /// Конфиг из одного файла (M1a): с `net #t` — те же строки, что нынешний gen1.
    const DEFAULT_VV: &str = r#"
# default.vv — конфиг ВЫЧИСЛЯЕТСЯ в те же service/shell-строки, что nix/system.nix
net = true
system(
  service("posixfs", "store:rw"),
  if net { service("net-srv", "dev:net:rw") } else { [] },
  shell("vsh",
        "endpoint:posixfs", "store:xw",
        if net { ["endpoint:net-srv"] } else { [] },
        "env"),
)
"#;

    const GEN1: &str = "service posixfs store:rw\n\
                        service net-srv dev:net:rw\n\
                        shell vsh endpoint:posixfs store:xw endpoint:net-srv env\n";
    const GEN2: &str = "service posixfs store:rw\n\
                        shell vsh endpoint:posixfs store:xw env\n";

    #[test]
    fn normalizes_full_config() {
        assert_eq!(build_config(DEFAULT_VV).expect("сборка"), GEN1);
    }

    #[test]
    fn net_off_matches_gen2() {
        let src = DEFAULT_VV.replace("net = true", "net = false");
        assert_eq!(build_config(&src).expect("сборка"), GEN2);
    }

    // ── M1b: import + слияние ────────────────────────────────────────────────

    /// Загрузчик модулей из карты в памяти + счётчик загрузок (для проверки кэша).
    struct MapLoader {
        mods: Vec<(&'static str, &'static str)>,
        loads: Cell<usize>,
    }
    impl MapLoader {
        fn new(mods: Vec<(&'static str, &'static str)>) -> Self {
            MapLoader { mods, loads: Cell::new(0) }
        }
    }
    impl ModuleLoader for MapLoader {
        fn load(&self, name: &str) -> Result<String, String> {
            self.loads.set(self.loads.get() + 1);
            self.mods
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, s)| String::from(*s))
                .ok_or_else(|| alloc::format!("нет модуля '{}'", name))
        }
    }

    /// Модульный конфиг из четырёх `.vv` воспроизводит gen1 ТОЧНО (import + append-слияние).
    #[test]
    fn imports_and_merges_to_gen1() {
        let loader = MapLoader::new(vec![
            ("services.vv", r#"[service("posixfs", "store:rw")]"#),
            ("networking.vv", r#"[service("net-srv", "dev:net:rw")]"#),
            (
                "shell.vv",
                r#"[shell("vsh", "endpoint:posixfs", "store:xw", "endpoint:net-srv", "env")]"#,
            ),
        ]);
        let default = r#"system(import("services.vv"),
                                import("networking.vv"),
                                import("shell.vv"))"#;
        assert_eq!(build_config_with(default, &loader).expect("сборка"), GEN1);
    }

    /// Модуль сам вычисляет свой вклад (define/if внутри модуля).
    #[test]
    fn module_can_compute_its_contribution() {
        let loader = MapLoader::new(vec![(
            "net.vv",
            r#"on = true
               if on { [service("net-srv", "dev:net:rw")] } else { [] }"#,
        )]);
        let out = build_config_with(r#"system(import("net.vv"))"#, &loader).expect("сборка");
        assert_eq!(out, "service net-srv dev:net:rw\n");
    }

    /// Один и тот же модуль, импортированный дважды, грузится РАЗ (кэш по имени).
    #[test]
    fn import_is_cached() {
        let loader = MapLoader::new(vec![(
            "svc.vv",
            r#"[service("posixfs", "store:rw")]"#,
        )]);
        let default = r#"system(import("svc.vv"), import("svc.vv"))"#;
        let out = build_config_with(default, &loader).expect("сборка");
        assert_eq!(out, "service posixfs store:rw\nservice posixfs store:rw\n");
        assert_eq!(loader.loads.get(), 1, "модуль должен грузиться один раз");
    }

    /// Циклический импорт детектируется, а не зависает.
    #[test]
    fn import_cycle_detected() {
        let loader = MapLoader::new(vec![
            ("a.vv", r#"import("b.vv")"#),
            ("b.vv", r#"import("a.vv")"#),
        ]);
        let err = build_config_with(r#"import("a.vv")"#, &loader).unwrap_err();
        assert!(err.contains("цикл"), "ожидали ошибку цикла, получили: {}", err);
    }

    /// Импорт отсутствующего модуля — ошибка от загрузчика.
    #[test]
    fn import_missing_errors() {
        let loader = MapLoader::new(vec![]);
        let err = build_config_with(r#"system(import("нет.vv"))"#, &loader).unwrap_err();
        assert!(err.contains("нет модуля"), "получили: {}", err);
    }

    /// Без загрузчика `import` — честная ошибка (не паника).
    #[test]
    fn import_without_loader_errors() {
        let err = build_config(r#"system(import("x.vv"))"#).unwrap_err();
        assert!(err.contains("загрузчик"), "получили: {}", err);
    }

    // ── настройки терминала в том же конфиге ─────────────────────────────────
    // `terminal`/`bind` — строки НЕ ядру, а `bin/term`. Конфиг поколения один: у системы одна
    // история и один откат, а кто какие строки читает — дело читателя.

    #[test]
    fn terminal_and_bind_normalize() {
        let src = r#"system(
                       service("posixfs", "store:rw"),
                       shell("term", "endpoint:posixfs", "mmio:fb"),
                       terminal("font-size", 18),
                       bind("normal", "C-a", "mode-pane"),
                       bind("pane", "|", "split-v"),
                     )"#;
        assert_eq!(
            build_config(src).expect("сборка"),
            "service posixfs store:rw\n\
             shell term endpoint:posixfs mmio:fb\n\
             terminal font-size 18\n\
             bind normal C-a mode-pane\n\
             bind pane | split-v\n"
        );
    }

    /// Веха 139 — обои объявляются записью `desktop`, и читает её композитор, а не ядро.
    /// Проверяем то, ради чего запись отдельная: имя картинки доезжает до строки поколения
    /// ЦЕЛИКОМ, и условие «обоев нет» выражается пустым списком, а не пустым именем.
    #[test]
    fn desktop_wallpaper_normalizes() {
        let src = r#"wall = "f/etc/фон.png"
                     system(
                       shell("wm", "mmio:fb"),
                       if wall == "" { [] } else { [desktop("wallpaper", wall)] },
                     )"#;
        assert_eq!(
            build_config(src).expect("сборка"),
            "shell wm mmio:fb\ndesktop wallpaper f/etc/фон.png\n"
        );
        let off = r#"wall = ""
                     system(shell("wm", "mmio:fb"),
                            if wall == "" { [] } else { [desktop("wallpaper", wall)] })"#;
        assert_eq!(build_config(off).expect("сборка"), "shell wm mmio:fb\n");
        // Опечатка в числе полей — ошибка СБОРКИ, а не молча пропущенная строка в загруженной
        // системе: ради этого этап нормализации и существует.
        assert!(build_config(r#"system(desktop("wallpaper"))"#).is_err());
    }

    /// Веха 144 — вид оболочки объявляется записью `ui`, читает её тулкит `void-ui`.
    ///
    /// Проверяем ровно то, ради чего запись отдельная от `desktop`: цвет доезжает строкой без
    /// изменений (решётка не съедается разбором), число печатается числом, а неполная запись —
    /// ошибка СБОРКИ. Иначе опечатка в теме означала бы панель, молча оставшуюся прежней.
    #[test]
    fn ui_theme_normalizes() {
        // Строка в двух решётках намеренно: внутри есть `"#` (цвет), и одной решётки не хватает.
        let src = r##"system(
                       shell("wm", "mmio:fb"),
                       ui("accent", "#4c7dfd"),
                       ui("scale", 150),
                       ui("font", "FiraMonoNerdFont-Regular.otf"),
                     )"##;
        assert_eq!(
            build_config(src).expect("сборка"),
            "shell wm mmio:fb\n\
             ui accent #4c7dfd\n\
             ui scale 150\n\
             ui font FiraMonoNerdFont-Regular.otf\n"
        );
        assert!(build_config(r##"system(ui("accent"))"##).is_err());

        // Так вид записан в шаблоне конфига: список с примерами под комментарием. Пустой он
        // обязан собираться молча — иначе свежая система не пересобралась бы вовсе, а причиной
        // была бы строка, которую человек даже не писал.
        let tmpl = r##"look = [
                         # ui("font", "FiraMonoNerdFont-Regular.otf"),
                         # ui("accent", "#4c7dfd"),
                       ]
                       system(shell("wm", "mmio:fb"), look)"##;
        assert_eq!(build_config(tmpl).expect("сборка"), "shell wm mmio:fb\n");
    }

    /// Веха 145.1 — «кто эта машина»: имя и аватар УСТРОЙСТВА, а не пользователя (их в VOID нет).
    ///
    /// Отдельной записью, а не ключом `ui`: это личность машины, и второй читатель у неё появится
    /// вместе с именем узла в сети — а сетевому стеку не место в настройках интерфейса.
    #[test]
    fn device_identity_normalizes() {
        let src = r#"system(
                       shell("wm", "mmio:fb"),
                       device("name", "voidbook"),
                       device("avatar", "f/etc/avatar.png"),
                     )"#;
        assert_eq!(
            build_config(src).expect("сборка"),
            "shell wm mmio:fb\n\
             device name voidbook\n\
             device avatar f/etc/avatar.png\n"
        );
        assert!(build_config(r#"system(device("name"))"#).is_err());

        // Пустой список из шаблона обязан собираться молча — как и `look`.
        let tmpl = r#"who = [
                        # device("name", "voidbook"),
                      ]
                      system(shell("wm", "mmio:fb"), who)"#;
        assert_eq!(build_config(tmpl).expect("сборка"), "shell wm mmio:fb\n");
    }

    /// Терминал живёт отдельным модулем и может целиком выключаться (как net.vv).
    #[test]
    fn terminal_module_can_be_off() {
        let loader = MapLoader::new(vec![(
            "terminal.vv",
            r#"on = false
               if on {
                 [shell("term", "mmio:fb"), bind("pane", "q", "quit")]
               } else {
                 [shell("vsh", "endpoint:posixfs")]
               }"#,
        )]);
        let out = build_config_with(r#"system(import("terminal.vv"))"#, &loader).expect("сборка");
        assert_eq!(out, "shell vsh endpoint:posixfs\n");
    }

    /// Опечатка в биндинге — ошибка СБОРКИ, а не молчаливо пропущенная строка на живой системе.
    #[test]
    fn bind_arity_checked_at_build() {
        let err = build_config(r#"system(bind("pane", "split-v"))"#).unwrap_err();
        assert!(err.contains("bind:"), "получили: {}", err);
    }

    // ── пакеты в конфиге (Веха 112) ──────────────────────────────────────────
    // Ещё один читатель того же текста: строки `packages …` берёт `pkg sync`.

    #[test]
    fn packages_normalize() {
        let src = r#"system(
                       service("posixfs", "store:rw"),
                       packages("hello", "jq"),
                     )"#;
        assert_eq!(
            build_config(src).expect("сборка"),
            "service posixfs store:rw\npackages hello jq\n"
        );
    }

    /// Список пакетов собирается из модулей, как и всё остальное: `packages` вливает список на
    /// любом месте, поэтому «база плюс своё» пишется одной строкой.
    #[test]
    fn packages_from_modules_merge() {
        let loader = MapLoader::new(vec![
            ("base.vv", r#"["hello", "jq"]"#),
            ("extra.vv", r#"[packages("curl")]"#),
        ]);
        let src = r#"base = import("base.vv")
                     system(packages(base, "xz"), import("extra.vv"))"#;
        let out = build_config_with(src, &loader).expect("сборка");
        assert_eq!(out, "packages hello jq xz\npackages curl\n");
    }

    /// `packages()` без имён — почти наверняка опечатка, и ловится она на сборке.
    #[test]
    fn packages_empty_is_error() {
        let err = build_config(r#"system(packages())"#).unwrap_err();
        assert!(err.contains("packages:"), "получили: {}", err);
    }

    /// Канал — часть конфига (Веха 113): «откуда система берёт софт» объявляется, а не зашито.
    #[test]
    fn channel_normalizes_and_is_single() {
        let src = r#"system(channel("https://channels.nixos.org/nixos-unstable"),
                            packages("hello"))"#;
        assert_eq!(
            build_config(src).expect("сборка"),
            "channel https://channels.nixos.org/nixos-unstable\npackages hello\n"
        );
        let err = build_config(r#"system(channel("a", "b"))"#).unwrap_err();
        assert!(err.contains("channel:"), "получили: {}", err);
    }

    /// Сеянный `packages.vv` собирается — и с пустым списком, и с именами. Проверка ровно того
    /// текста, который пишет `init-config`: он длиннее прочих модулей, и опечатка в нём
    /// проявилась бы только на живой машине.
    #[test]
    fn seeded_packages_module_shape() {
        let module = |want: &str| {
            alloc::format!(
                "want = {}\n\nsource = \"https://ch/nixos-unstable\"\n\n\
                 append(\n  [channel(source)],\n  if null?(want) {{ [] }} else {{ [packages(want)] }},\n)\n",
                want
            )
        };
        let loader = MapLoader::new(vec![("packages.vv", module("[]").leak() as &str)]);
        assert_eq!(
            build_config_with(r#"system(import("packages.vv"))"#, &loader).expect("пусто"),
            "channel https://ch/nixos-unstable\n"
        );
        let loader = MapLoader::new(vec![("packages.vv", module(r#"["hello", "which"]"#).leak() as &str)]);
        assert_eq!(
            build_config_with(r#"system(import("packages.vv"))"#, &loader).expect("с именами"),
            "channel https://ch/nixos-unstable\npackages hello which\n"
        );
    }
}
