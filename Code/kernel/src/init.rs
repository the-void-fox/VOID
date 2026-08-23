//! Веха 40 — декларативный init: система поднимается по КОНФИГУ из store, а не по сценарию,
//! зашитому в `kmain`. «configuration.nix родными средствами»: какие серверы запускать, какие
//! права им дать, что сделать shell'ом — описано текстом-объектом; ядро его читает и исполняет.
//!
//! Модель — ровно как у NixOS ([[0004-void-pkg]]): язык Nix (если нужен) вычисляется на ХОСТЕ и
//! кладёт готовый конфиг в store через мост; работающая система Nix НЕ исполняет — читает
//! уже-вычисленный результат. Поколения — история корня, откат — смена корня (`system/current`):
//! `switch NAME` в vsh + перезагрузка = загрузка в другую конфигурацию, и назад.
//!
//! Формат конфига (текст, редактируемый из vsh И генерируемый из `nix/system.nix`):
//! ```text
//! # комментарий
//! service posixfs store:rw          # сервер + права a0
//! service net-srv dev:net:rw
//! shell   vsh endpoint:posixfs store:rwx endpoint:net-srv env
//! ```
//! Токены прав: `store:RWX`, `dev:net:RW`, `dev:block:RW`, `endpoint:ИМЯ[:S]` (по умолчанию SEND),
//! `power` (право выключить машину, Веха 101), `env` (передать ARCH/SYSTEM). Буквы прав: `r`=READ `w`=WRITE `x`=EXEC `s`=SEND `g`=GRANT.
//! Права по порядку → `a0`, `a1`, и все → таблица стартовых capability (как контракт Вехи 30).
//! Отдельно от прав — `arg:СТРОКА` (Веха 92): настройка сервиса, уходит в его argv
//! (`service net-srv dev:net:rw arg:dhcp=off arg:ip=10.0.2.15/24`).
//!
//! Вехи 112–113 — `packages имя…` и `channel url` читает `pkg` (что система обязана иметь и
//! откуда берёт).
//! Веха 100 — в конфиге бывают строки НЕ ядру: `terminal ключ значение` и
//! `bind режим клавиша действие` читает терминал (`bin/term` берёт активное поколение из store
//! сам); Веха 139 — `desktop …` композитор; Веха 144 — `ui …` тулкит оболочки. Ядро их
//! пропускает: конфигурация системы — одна вещь с одной историей поколений, а кто какие строки
//! из неё берёт — дело читателя.

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use void_abi::Rights;

use crate::{arch, cap, object, println, proc};

/// Корень-указатель активного поколения: его содержимое — ИМЯ поколения (`system/<имя>`).
const CURRENT_ROOT: &str = "system/current";

/// Конфиг по умолчанию (поколение `gen1`) — полный: файлы, сеть, интерактивный shell.
/// Совпадает с тем, что до Вехи 40 было зашито в `shell_session`.
const DEFAULT_GEN1: &str = "\
# VOID — поколение по умолчанию (полное: файлы + сеть)
service posixfs store:rw
service net-srv dev:net:rw
shell vsh endpoint:posixfs store:rwx endpoint:net-srv power env
";

/// Второе поколение (`gen2`) — минимальное, БЕЗ сети: витрина отката. Тот же shell, но без
/// net-srv и без сетевого эндпоинта → `ping` в vsh честно говорит «сети нет».
const DEFAULT_GEN2: &str = "\
# VOID — минимальное поколение (без сети)
service posixfs store:rw
shell vsh endpoint:posixfs store:rwx power env
";

/// Третье поколение (`gen3`, Веха 97) — **терминал на настоящих глифах** вместо текстового
/// шелла: `term` получает экран под capability (`mmio:fb`) и рисует сам. Отдельным поколением,
/// а не заменой gen1, ровно потому, для чего поколения и делались: новое можно попробовать и
/// откатиться, не потеряв рабочую систему. Пиксельного режима может не быть (загрузка PVH,
/// riscv) — тогда `term` честно скажет об этом в serial и выйдет.
///
/// **Порядок прав значим** (Веха 99): дети наследуют его как есть, а `vvsh` ждёт эндпоинт
/// файлов первым и store вторым. Поэтому экран стоит ПОСЛЕ них — сам `term` своё право находит
/// перебором и от порядка не зависит, а шеллу в панели порядок важен.
const DEFAULT_GEN3: &str = "\
# VOID — поколение с графическим терминалом (Веха 97)
service posixfs store:rw
service net-srv dev:net:rw
shell term endpoint:posixfs store:rwx mmio:fb power env
";

/// Четвёртое поколение (`gen4`, Веха 129) — **оконный режим**: композитор владеет экраном, а
/// терминал живёт в нём окном. `arg:term` — клиент, которого `wm` открывает на старте.
///
/// Системное и переписывается на каждой загрузке, как `gen3`, и по той же причине: это витрина
/// возможностей образа, а не выбор владельца. До него оконный режим существовал, но добраться до
/// него можно было только правкой конфига руками (`init-config` → `mode = "wm"` → `rebuild`) —
/// то есть графическая оболочка была недостижима из коробки, и проверять её приходилось,
/// набирая конфиг в редакторе по serial. Выбор по-прежнему за владельцем: `switch gen4`.
const DEFAULT_GEN4: &str = "\
# VOID — оконный режим: композитор + терминал окном (Веха 129)
service posixfs store:rw
service net-srv dev:net:rw
shell wm endpoint:posixfs store:rwx mmio:fb power env arg:term
";

/// Прочитать текстовый объект по корню-имени. `None` — корня нет или это не UTF-8.
fn read_text(root: &str) -> Option<String> {
    let id = object::root(root)?;
    object::with(&id, |b| b.and_then(|x| core::str::from_utf8(x).ok()).map(String::from))
}

/// Записать текст объектом и привязать к корню (сев конфига/поколения).
fn write_text(root: &str, text: &str) {
    let id = object::put(text.as_bytes());
    object::set_root(root, id);
}

/// Разобрать буквенный набор прав (`rwxsg`) в [`Rights`]. Неизвестные буквы игнорируются.
fn parse_rights(s: &str) -> Rights {
    let mut r = Rights::NONE;
    for c in s.chars() {
        r = r.union(match c {
            'r' => Rights::READ,
            'w' => Rights::WRITE,
            'x' => Rights::EXEC,
            's' => Rights::SEND,
            'g' => Rights::GRANT,
            _ => Rights::NONE,
        });
    }
    r
}

/// Запустить программу из store по имени (арх-корень → content-id → ELF → процесс). Личность
/// процесса (`pname`) — имя из конфига, «утёкшее» в `'static` (устойчиво по СОДЕРЖИМОМУ: домен
/// `.cspace` переиспользуется по имени). `None` — программы нет в store или ELF негоден.
fn spawn(name: &str) -> Option<usize> {
    let root = crate::prog_root(name);
    let id = object::root(&root)?;
    let bytes = object::with(&id, |b| b.map(<[u8]>::to_vec))?;
    let pname: &'static str = Box::leak(name.to_string().into_boxed_str());
    match proc::spawn_elf(pname, &bytes, 0) {
        Ok(pid) => Some(pid),
        Err(e) => {
            println!("  [init] '{}': негодный ELF: {:?}", name, e);
            None
        }
    }
}

/// Сминтить capability по токену конфига в домен процесса `pid`. `services` — уже поднятые
/// серверы (имя → pid) для разрешения `endpoint:ИМЯ`. Возвращает дескриптор или `None`
/// (`env` — не capability; неизвестный/несогласованный токен — предупреждение).
fn mint_cap(pid: usize, token: &str, services: &[(String, usize)]) -> Option<usize> {
    let dom = proc::domain(pid);
    if let Some(r) = token.strip_prefix("store:") {
        Some(cap::mint(dom, cap::Target::Store, parse_rights(r)).bits() as usize)
    } else if let Some(r) = token.strip_prefix("dev:net:") {
        Some(cap::mint(dom, cap::Target::Device(cap::Device::Net), parse_rights(r)).bits() as usize)
    } else if let Some(r) = token.strip_prefix("dev:block:") {
        Some(cap::mint(dom, cap::Target::Device(cap::Device::Block), parse_rights(r)).bits() as usize)
    } else if token == "power" {
        // Веха 101 — право выключить машину. Обычно у шелла: `exit`/`poweroff` должны
        // действительно снимать питание, а не только закрывать программу.
        Some(cap::mint(dom, cap::Target::Power, Rights::WRITE).bits() as usize)
    } else if token == "dma" {
        // Веха 51 — право выделять DMA-память (userspace-драйверу под кольца/буферы).
        Some(cap::mint(dom, cap::Target::Dma, Rights::WRITE).bits() as usize)
    } else if let Some(dev) = token.strip_prefix("mmio:") {
        // Веха 51 — окно MMIO устройства: найти его на PCI, отдать (физ. база + длина).
        let region = match dev {
            "e1000" => crate::arch::probe_e1000().map(|base| (base, 0x20000usize)),
            // Веха 132 — Atheros AR8151 (X54C). Ищется НА ЛЮБОЙ ШИНЕ: карта сидит за мостом
            // PCIe, и обход одной шины её не находил. Окно регистров у этих карт — 256 КиБ.
            #[cfg(target_arch = "x86_64")]
            "atl1c" => crate::arch::probe_bar0(0x1969, 0x1083, 0x40000).map(|b| (b, 0x40000usize)),
            // Веха 97 — ЭКРАН как обычное устройство под capability: терминал получает окно
            // фреймбуфера и рисует сам. Ядро при этом умолкает (см. fb::give_to_user).
            "fb" => crate::arch::video_window(),
            _ => None,
        };
        match region {
            Some((base, len)) => Some(
                cap::mint(dom, cap::Target::Mmio { base, len }, Rights::READ.union(Rights::WRITE))
                    .bits() as usize,
            ),
            None => {
                println!("  [init] mmio:{} — устройство не найдено (пропуск)", dev);
                None
            }
        }
    } else if let Some(rest) = token.strip_prefix("endpoint:") {
        // endpoint:ИМЯ  или  endpoint:ИМЯ:права (по умолчанию SEND).
        let (svc, rights) = match rest.split_once(':') {
            Some((s, r)) => (s, parse_rights(r)),
            None => (rest, Rights::SEND),
        };
        match services.iter().find(|(n, _)| n == svc) {
            Some((_, spid)) => {
                Some(cap::mint(dom, cap::Target::Endpoint(*spid), rights).bits() as usize)
            }
            None => {
                println!("  [init] endpoint:{} — нет такого сервера (пропуск)", svc);
                None
            }
        }
    } else {
        None
    }
}

/// Разобрать и исполнить конфиг: поднять каждую запись (`service`/`shell`), сминтить её права,
/// разложить их как `a0`/`a1`/стартовые (контракт Вехи 30). Первый два права дублируются в
/// регистры запуска — как раньше делал `shell_session` руками.
fn apply(config: &str) -> Vec<(String, usize)> {
    apply_with(config, Vec::new())
}

/// То же, но со СПИСКОМ уже работающих сервисов: спасательный шелл поднимается поверх них
/// (Веха 119.1), а не рядом со вторым файловым сервером.
fn apply_with(config: &str, known: Vec<(String, usize)>) -> Vec<(String, usize)> {
    let mut services: Vec<(String, usize)> = known; // имя → pid (для endpoint:)
    let env = alloc::format!("ARCH={}\0SYSTEM=void\0", arch::ARCH_NAME);

    for entry in void_conf::entries(config) {
        let kind = entry.kind;
        // Веха 100 — конфиг поколения ОДИН, а читателей много: у системы одна история и один
        // откат, поэтому настройки терминала, композитора, тулкита и пакетов живут в том же
        // тексте. Ядро берёт свои строки, прочие пропускает молча — это не ошибка конфига.
        //
        // Веха 148.8 — какие строки чьи, знает не ядро, а СЛОВАРЬ ([`void_conf::KINDS`]).
        // Раньше список чужих видов был записан здесь, и каждый новый вид требовал правки ЯДРА:
        // до неё система на каждой загрузке звала его «неизвестной директивой».
        let Some(k) = void_conf::kind(kind) else {
            println!("  [init] неизвестная директива '{}' (пропуск)", kind);
            continue;
        };
        if !k.kernel {
            continue;
        }
        let mut tok = entry.words();
        let Some(name) = tok.next() else { continue };
        let Some(pid) = spawn(name) else { continue };

        // Права по порядку: собрать дескрипторы, разложить в a0/a1 + стартовую таблицу.
        let mut caps: Vec<usize> = Vec::new();
        let mut names: Vec<alloc::string::String> = Vec::new();
        let mut want_env = false;
        let mut nargs = 0usize;
        for t in tok {
            if t == "env" {
                want_env = true;
            } else if let Some(a) = t.strip_prefix("arg:") {
                // Веха 92: НЕ capability, а настройка — уходит в argv процесса. Права отвечают
                // на «что процессу можно», аргументы — на «как ему себя вести»; смешивать их в
                // одном токене было бы враньём про cap-модель.
                if proc::push_arg(pid, a) {
                    nargs += 1;
                } else {
                    println!("  [init] arg:{} — argv переполнен (пропуск)", a);
                }
            } else if let Some(bits) = mint_cap(pid, t, &services) {
                // Веха 99.1 — запоминаем ИМЯ права вместе с его позицией: ниже они уйдут в
                // окружение процесса. Позиция сама по себе — плохой контракт (см. ниже).
                names.push(cap_name(t));
                caps.push(bits);
            }
        }
        if let Some(&a0) = caps.first() {
            proc::set_arg(pid, a0);
        }
        if let Some(&a1) = caps.get(1) {
            proc::set_arg2(pid, a1);
        }
        for &c in &caps {
            proc::push_start_cap(pid, c);
        }
        // Веха 99.1 — **ИМЕНА ПРАВ В ОКРУЖЕНИИ**. До сих пор стартовые права были только
        // позиционными: программа знала «файловый сервер — нулевой, store — первый». Это
        // выстрелило ровно так, как и должно было: в конфиге поменяли порядок токенов, и `vvsh`
        // принял фреймбуфер за файловый сервер — `ls` роняла панель, `run` не запускал.
        // Позиция остаётся (совместимость, a0/a1), но теперь рядом есть имя:
        //   `CAP_POSIXFS=1 CAP_STORE=2 CAP_FB=0`
        // Тот же приём, что и `STDIO=<i>` для чужого stdio: ядро кладёт СТРОКУ, смысл её —
        // дело userspace.
        let mut env_full = alloc::string::String::new();
        if want_env {
            env_full.push_str(&env);
        }
        for (i, n) in names.iter().enumerate() {
            if !n.is_empty() {
                env_full.push_str(&alloc::format!("CAP_{}={}\0", n, i));
            }
        }
        if !env_full.is_empty() {
            proc::set_env(pid, env_full.as_bytes());
        }
        println!(
            "  [init] {} P{} '{}' — прав {}{}{}",
            kind, pid, name, caps.len(),
            if want_env { " +env" } else { "" },
            if nargs > 0 { " +args" } else { "" },
        );
        if kind == "service" {
            services.push((name.to_string(), pid));
        }
    }
    services
}

/// Веха 99.1 — короткое ИМЯ права по токену конфига: `endpoint:posixfs` → `POSIXFS`,
/// `store:rwx` → `STORE`, `mmio:fb` → `FB`, `dev:net:rw` → `NET`. Пустая строка — имени нет
/// (право останется только позиционным).
fn cap_name(token: &str) -> alloc::string::String {
    let base = if let Some(r) = token.strip_prefix("endpoint:") {
        r.split(':').next().unwrap_or(r)
    } else if token.starts_with("store:") {
        "store"
    } else if let Some(d) = token.strip_prefix("dev:") {
        d.split(':').next().unwrap_or(d)
    } else if let Some(d) = token.strip_prefix("mmio:") {
        d
    } else if token == "dma" {
        "dma"
    } else if token == "power" {
        "power"
    } else {
        ""
    };
    // Имя переменной окружения: верхний регистр, дефисы в подчёркивания.
    base.chars()
        .map(|c| if c == '-' { '_' } else { c.to_ascii_uppercase() })
        .collect()
}

/// Поднять ОДНУ строку конфига поверх уже работающих сервисов (спасательный шелл).
///
/// Отдельная функция, а не второй `apply`: сервисы уже запущены, и поднимать их заново значило
/// бы получить два файловых сервера на один store.
fn apply_shell(line: &str, services: &[(String, usize)]) {
    apply_with(line, services.to_vec());
}

/// Веха 40 — точка входа декларативной загрузки (заменяет зашитый `shell_session`). Читает
/// активное поколение из `system/current` (сеет два поколения по умолчанию на чистом диске),
/// исполняет его конфиг и отдаёт управление планировщику до выхода shell'а.
pub fn boot() {
    // Чистый диск: посеять поколения по умолчанию и выбрать полное.
    if object::root("system/gen1").is_none() {
        write_text("system/gen1", DEFAULT_GEN1);
        write_text("system/gen2", DEFAULT_GEN2);
        write_text(CURRENT_ROOT, "gen1");
        println!("  [init] чистый диск — посеяны поколения gen1 (полное) и gen2 (без сети)");
    }

    // Веха 99.1 — `gen3` СИСТЕМНОЕ и переписывается на каждой загрузке, в отличие от gen1/gen2
    // (те принадлежат владельцу и правятся `rebuild`). Причина конкретная: у поколения был
    // неверный ПОРЯДОК прав, а стартовые права позиционные — дети наследуют порядок, и `vvsh`
    // принимал фреймбуфер за файловый сервер (`ls` роняла панель, `run` не запускал). Исправить
    // это в образе мало: конфиг лежит в СТОРЕ и переживает обновление ядра, поэтому машины,
    // успевшие попробовать терминал, остались бы сломанными навсегда.
    //
    // Общий вывод записан отдельно ([[known-gaps]]): позиционные права — ловушка, и правильное
    // лечение — искать право ПО ИМЕНИ (`CAP_<ИМЯ>` в окружении, уже отдаётся выше).
    if read_text("system/gen3").as_deref() != Some(DEFAULT_GEN3) {
        write_text("system/gen3", DEFAULT_GEN3);
        println!("  [init] поколение 'gen3' (системное) обновлено из образа ядра");
    }
    if read_text("system/gen4").as_deref() != Some(DEFAULT_GEN4) {
        write_text("system/gen4", DEFAULT_GEN4);
        println!("  [init] поколение 'gen4' (оконный режим) обновлено из образа ядра");
    }

    // Активное поколение: system/current → имя → system/<имя> → текст конфига.
    let gen = read_text(CURRENT_ROOT).unwrap_or_else(|| "gen1".to_string());
    let gen = gen.trim().to_string();
    let config = match read_text(&alloc::format!("system/{}", gen)) {
        Some(c) => c,
        None => {
            println!("  [init] поколение '{}' не найдено — беру gen1", gen);
            read_text("system/gen1").unwrap_or_else(|| DEFAULT_GEN1.to_string())
        }
    };
    println!("  [init] поколение '{}' — поднимаю систему по конфигу:", gen);
    let services = apply(&config);

    // Веха 51–54 — хостируемый userspace-драйвер e1000: если карта не занята ядром (в VM сеть на
    // virtio-net), поднять её драйвер В USERSPACE поверх шима lx_emul. Веха 54 — если на диск мостом
    // импортирован C-драйвер `lx_e1000_c` (C-путь lx_emul), поднять ЕГО; иначе Rust-каркас
    // `lx_e1000` (Веха 53). Права те же: MMIO-cap на регистры, DMA-cap, IRQ-cap (прерывание карты →
    // VEC_USERDRV). Тихо пропускается, если e1000 нет. (Сырой демо Вех 51–52 — bin/e1000d, образец.)
    // Веха 135.3 — «если карта не занята ядром» было написано в комментарии, но НЕ ПРОВЕРЯЛОСЬ:
    // условием стояло одно лишь наличие карты. В обычном прогоне это не всплывало (в VM сеть на
    // virtio-net, и e1000 действительно оставалась свободной), а стоило запустить машину с одной
    // только e1000 — и на неё садились ДВА драйвера сразу: встроенный в ядро и хостируемый.
    //
    // Выглядело это как «встроенный e1000 не работает»: первый кадр уходил, ответ приходил, а
    // дальше на проводе появлялись чужие кадры (широковещательный кадр с ethertype 0x88b5 и
    // начинкой из 0x56 — это демо `bin/e1000d`), кольцо передачи переставало слушаться, и DHCP
    // не доходил. Драйвер был исправен; за карту дрались.
    if arch::probe_e1000().is_some() && !crate::e1000::present() {
        // Веха 69 — предпочесть ПОРТИРОВАННЫЙ e1000 (неизменённый e1000_hw.c ядра Linux через
        // MMIO-cap, `lx-e1000-hw`), если импортирован; иначе C-драйвер Вехи 54, иначе Rust-каркас.
        let (driver, started) = match spawn("lx-e1000-hw") {
            Some(pid) => ("lx-e1000-hw", Some(pid)),
            None => match spawn("lx_e1000_c") {
                Some(pid) => ("lx_e1000_c", Some(pid)),
                None => ("lx_e1000", spawn("lx_e1000")),
            },
        };
        if let Some(pid) = started {
            match (mint_cap(pid, "mmio:e1000", &[]), mint_cap(pid, "dma", &[])) {
                (Some(m), Some(d)) => {
                    proc::set_arg(pid, m);
                    proc::set_arg2(pid, d);
                    proc::push_start_cap(pid, m);
                    proc::push_start_cap(pid, d);
                    // IRQ-cap: замаршрутизировать прерывание e1000 на VEC_USERDRV, отдать драйверу
                    // третьим стартовым правом (шим ждёт его в request_irq → SYS_IRQ_WAIT). start_cap(2).
                    let irq = arch::e1000_irq_setup().map(|vec| {
                        cap::mint(proc::domain(pid), cap::Target::Irq { vector: vec }, Rights::READ)
                            .bits() as usize
                    });
                    if let Some(i) = irq {
                        proc::push_start_cap(pid, i);
                    }
                    println!(
                        "  [init] userspace-драйвер {} P{} (на lx_emul) — выданы MMIO+DMA{} cap",
                        driver,
                        pid,
                        if irq.is_some() { "+IRQ" } else { "" },
                    );
                }
                _ => println!("  [init] {}: не удалось выдать MMIO/DMA cap (пропуск)", driver),
            }
        }
    }

    // Веха 132 — Atheros AR8151 (проводная карта X54C) как userspace-драйвер поверх
    // ПОРТИРОВАННОГО кода Linux. Пока это харнесс первого контакта: MMIO-права хватает, чтобы
    // прочитать EEPROM, MAC и PHY вендорными функциями. DMA и прерывание появятся вместе с
    // кольцами дескрипторов.
    //
    // Тихо пропускается, если карты нет: в QEMU её не эмулируют вовсе, и это нормальный случай,
    // а не ошибка. На X54C, наоборот, отсутствие строки ниже само по себе диагноз.
    #[cfg(target_arch = "x86_64")]
    if let Some(base) = arch::probe_bar0(0x1969, 0x1083, 0x40000) {
        println!("  [init] найдена Atheros AR8151 (1969:1083), регистры {:#x}", base);
        // Веха 133 — предпочесть ПОЛНЫЙ драйвер (настоящий `atl1c_probe`, кольца на DMA); если
        // его нет в образе, поднять харнесс первого контакта (Веха 132: регистры, MAC, PHY).
        let (name, started) = match spawn("lx-atl1c-full") {
            Some(pid) => ("lx-atl1c-full", Some(pid)),
            None => ("lx-atl1c-hw", spawn("lx-atl1c-hw")),
        };
        match started {
            Some(pid) => {
                // MMIO — окно регистров, DMA — кольца дескрипторов, IRQ — приём без опроса.
                // Права те же и в том же порядке, что у портированного e1000 (Вехи 69–72):
                // start_cap 0/1/2. Харнессу первого контакта лишние права не мешают — он их
                // просто не берёт.
                match (mint_cap(pid, "mmio:atl1c", &[]), mint_cap(pid, "dma", &[])) {
                    (Some(m), Some(d)) => {
                        proc::set_arg(pid, m);
                        proc::set_arg2(pid, d);
                        proc::push_start_cap(pid, m);
                        proc::push_start_cap(pid, d);
                        // IRQ-право третьим (start_cap 2), как у e1000: без него драйвер не
                        // узнает о приходе кадра и остался бы с опросом.
                        let irq = arch::intx_irq_setup(0x1969, 0x1083).map(|vec| {
                            cap::mint(proc::domain(pid), cap::Target::Irq { vector: vec },
                                      Rights::READ).bits() as usize
                        });
                        if let Some(i) = irq {
                            proc::push_start_cap(pid, i);
                        }
                        println!(
                            "  [init] драйвер {} P{} — выданы MMIO+DMA{} права",
                            name, pid, if irq.is_some() { "+IRQ" } else { "" },
                        );
                    }
                    _ => println!("  [init] {}: MMIO/DMA права выдать не удалось", name),
                }
            }
            // Драйвер едет семенем в образе ядра (Веха 132.1). Нет его — карта без драйвера.
            None => println!("  [init] AR8151 есть, а драйвера в образе нет"),
        }
    }

    let started = crate::clock::uptime_ns();
    proc::run();
    let lived = crate::clock::uptime_ns().saturating_sub(started);
    println!("  [init] сессия '{}' завершена (shell вышел) — обратно в ядро", gen);

    // Веха 119.1 — СТРАХОВКА от поколения, которое не поднимается.
    //
    // Графический шелл может не запуститься по причинам, о которых конфиг не знает: нет
    // фреймбуфера (загрузка без видеорежима), не хватило памяти, программа упала на старте.
    // Раньше это означало систему БЕЗ ШЕЛЛА: выбрать другое поколение нечем, потому что
    // выбирают его командой, а команду ввести некуда. Машина превращалась в кирпич, чинимый
    // только с другого компьютера — проверено на себе.
    //
    // Признак беды выбран простой и честный: шелл прожил меньше пяти секунд. Живой шелл, из
    // которого человек вышел сам, столько не живёт разве что при мгновенном `exit` — и тогда
    // спасательный `vsh` ему не помешает.
    if lived < 5_000_000_000 {
        println!(
            "  [init] шелл поколения '{}' продержался {} мс — поднимаю спасательный vsh",
            gen,
            lived / 1_000_000
        );
        println!("  [init] почему так вышло — смотри выше в этом же журнале (`klog`)");
        apply_shell("shell vsh endpoint:posixfs store:rwx power env", &services);
        proc::run();
        println!("  [init] спасательная сессия завершена — обратно в ядро");
    }
}
