//! Язык интерфейса (Веха 178): русский и английский.
//!
//! ## Ключ — сама русская строка
//!
//! Словаря условных ключей (`fm.menu.delete`) здесь нет намеренно. Ключ — это ТЕКСТ, который
//! написан в программе, а перевод ищется по нему:
//!
//! ```ignore
//! u.label(r, t("удалить"), th.danger, Align::Left);
//! ```
//!
//! Что это даёт. Во-первых, программа читаема без словаря: в коде видно, что появится на экране,
//! а не `Msg::DeleteConfirm`. Во-вторых, **потерянный перевод деградирует, а не ломается**: если
//! строку в коде поменяли, а в таблицу не занесли, английский покажет русский текст — заметно и
//! не страшно. С условными ключами та же ошибка даёт на экране `fm.menu.delete`, то есть мусор,
//! которого никто не писал.
//!
//! Цена честная и одна: русский текст в коде обязан совпадать с ключом в таблице ЗНАК В ЗНАК.
//!
//! ## Порядок слов бывает разный
//!
//! Поэтому переводится не кусок фразы, а фраза целиком — с местами подстановки:
//!
//! ```ignore
//! f2(t("удалено {} из {}"), &ok.to_string(), &all.to_string())
//! ```
//!
//! Собирать предложение из переведённых обрывков (`t("удалено") + число + t("из")`) нельзя: в
//! другом языке порядок другой, и обрывки станут не тем предложением. Это ровно та ошибка, из-за
//! которой локализация обычно и выглядит машинной.
//!
//! ## Чего здесь нет
//!
//! **Журнала ядра и диагностики в консоль.** Они остаются русскими: их читает тот, кто чинит
//! систему, а не тот, кто ею пользуется, и переводить их значило бы удваивать работу ради
//! читателя, которого нет. Граница проходит по вопросу «увидит ли это человек в окне».
//!
//! **Множественного числа.** «1 файл / 2 файла / 5 файлов» здесь не различаются: правил склонения
//! в таблице нет, а изобретать половину ICU ради трёх мест не стоит. Там, где это заметно, фраза
//! написана так, чтобы число стояло отдельно («файлов: 5»).
//!
//! **Загрузки перевода файлом.** Таблица вкомпилирована в каждую программу (десятки килобайт), и
//! это осознанный размен: перевод из store был бы ещё одним источником отказа на пути к первому
//! кадру, а размер здесь не дефицит. Так же поступили со встроенным шрифтом (Веха 114).

use core::sync::atomic::{AtomicU8, Ordering};

/// Язык интерфейса. Номер, а не строка: его читают из каждого кадра.
const RU: u8 = 0;
const EN: u8 = 1;

static LANG: AtomicU8 = AtomicU8::new(RU);

/// Веха 178 — язык из конфига: `ui language en`. Зовётся один раз, на старте программы.
///
/// Незнакомое значение — русский: интерфейс обязан подняться на чём угодно, а опечатку ловит
/// `rebuild` (там же, где остальные ключи `ui`).
pub fn set_from_config(text: &str) {
    let lang = void_conf::get(text, "ui", "language").unwrap_or("ru");
    LANG.store(if lang.starts_with("en") { EN } else { RU }, Ordering::Relaxed);
}

/// Английский ли сейчас интерфейс. Нужно там, где переводом дело не ограничивается: формат даты,
/// раскладка колонок.
pub fn is_en() -> bool {
    LANG.load(Ordering::Relaxed) == EN
}

/// Перевести строку интерфейса. Русский — тождество и НИ ОДНОГО сравнения: за язык по умолчанию
/// не платит никто.
///
/// Принимает `&'static str` намеренно: переводится только то, что написано в программе. Строку,
/// собранную во время работы, в таблице искать бессмысленно — для неё есть места подстановки
/// (`ui::f1`/`f2`/`f3`).
pub fn t(s: &'static str) -> &'static str {
    if LANG.load(Ordering::Relaxed) == RU {
        return s;
    }
    en(s)
}

/// Таблица переводов. Не найдено — отдаём русский: см. шапку про деградацию.
///
/// Порядок разделов — по программам, чтобы правку было где искать. Строки внутри раздела идут в
/// том порядке, в каком встречаются на экране, а не по алфавиту: так видно фразу целиком.
fn en(s: &'static str) -> &'static str {
    match s {
        // ── панель ─────────────────────────────────────────────────────────────────────────
        "Уведомления" => "Notifications",
        "нет уведомлений" => "no notifications",
        "ещё {}" => "{} more",
        "сборка" => "build",
        "поколение" => "generation",
        "время" => "time",
        "дата" => "date",
        "столов" => "desktops",
        "корней в store" => "roots in store",
        "нажми ещё раз" => "press again",
        "{}, сейчас {}" => "{}, now on {}",
        "{} ч {} мин" => "{} h {} min",
        "{} мин" => "{} min",
        "{} с" => "{} s",

        // ── строка запуска ─────────────────────────────────────────────────────────────────
        "имя программы" => "program name",
        "Enter запустит «{}»" => "Enter runs \"{}\"",
        "{} из {} программ store" => "{} of {} store programs",
        "ярлыков: {}" => "shortcuts: {}",
        "{} из {} ярлыков" => "{} of {} shortcuts",
        "приложение" => "application",
        "программа store, без ярлыка" => "store program, no shortcut",
        "строка запуска" => "launcher",

        // ── файловый менеджер ──────────────────────────────────────────────────────────────
        "нет права на файловый сервер" => "no right to the file server",
        "каталог не открылся: {}" => "could not open folder: {}",
        "путь" => "path",
        "поиск: {}" => "search: {}",
        "закладки" => "places",
        "имя каталога" => "folder name",
        "имя файла" => "file name",
        "новое имя" => "new name",
        "проверь путь и права" => "check the path and the rights",
        "каталог пуст" => "folder is empty",
        "ничего не нашлось" => "nothing found",
        "каталог" => "folder",
        "показаны не все: сервер отдал имён — {}" => "not all shown: the server gave {} names",
        "это уже здесь" => "already here",
        "перенесено: {}" => "moved: {}",
        "не перенести: {}" => "could not move: {}",
        "перенесено: {}, не вышло: {}" => "moved: {}, failed: {}",
        "в конфиге нет строки `default {} …`" => "no `default {} …` line in the config",
        "нет права на store — запускать нечем" => "no store right — nothing to launch with",
        "не запустить: {}" => "could not launch: {}",
        "закладки не записать" => "could not save places",
        "в закладки кладём каталоги" => "places are for folders",
        "уже в закладках" => "already in places",
        "добавлено в закладки" => "added to places",
        "закладка убрана" => "place removed",
        "имя без косых черт и не пустое" => "a name without slashes, and not empty",
        "переименовано: {}" => "renamed: {}",
        "переименовать не вышло" => "rename failed",
        "создано: {}" => "created: {}",
        "создать не вышло" => "could not create",
        "возвращено {} из {}" => "restored {} of {}",
        "в корзину: {} из {}" => "to trash: {} of {}",
        "корзина очищена" => "trash emptied",
        "корзину не очистить" => "could not empty the trash",
        "нечего брать" => "nothing to take",
        "взято: {} — откройте каталог и «перенести сюда»"
            => "taken: {} — open a folder and choose \"move here\"",
        "взято: {} — откройте каталог и «скопировать сюда»"
            => "taken: {} — open a folder and choose \"copy here\"",
        "удалено: {}" => "deleted: {}",
        "не удалить: {}" => "could not delete: {}",
        "удалено: {}, не вышло: {}" => "deleted: {}, failed: {}",
        "скопировано: {}" => "copied: {}",
        "буфер обмена недоступен (нет права на store?)"
            => "clipboard unavailable (no store right?)",
        "не вышло: {}" => "failed: {}",
        "готово: {}, не вышло: {}" => "done: {}, failed: {}",
        "буфер обмена пуст" => "the clipboard is empty",
        "это не путь: {}" => "not a path: {}",
        "нет такого пути: {}" => "no such path: {}",
        "объектов: {}" => "{} objects",
        "открыть" => "open",
        "переименовать" => "rename",
        "копировать путь" => "copy path",
        "скопировать это" => "copy these",
        "скопировать" => "copy",
        "перенести это" => "cut these",
        "перенести" => "cut",
        "открыть в терминале" => "open in terminal",
        "в закладки" => "add to places",
        "вернуть на место" => "put back",
        "в корзину" => "move to trash",
        "удалить ВМЕСТЕ С СОДЕРЖИМЫМ? ещё раз" => "delete WITH ALL CONTENTS? once more",
        "удалить навсегда? ещё раз" => "delete permanently? once more",
        "удалить навсегда" => "delete permanently",
        "убрать закладку" => "remove place",
        "очистить корзину? ещё раз" => "empty the trash? once more",
        "очистить корзину" => "empty the trash",
        "обновить" => "refresh",
        "создать каталог" => "new folder",
        "создать файл" => "new file",
        "перенести сюда ({})" => "move here ({})",
        "скопировать сюда ({})" => "copy here ({})",
        "вставить путь" => "paste path",
        "копировать путь каталога" => "copy folder path",
        "этот каталог в закладки" => "add this folder to places",
        "удалить НАВСЕГДА вместе с содержимым? ещё раз Delete"
            => "delete PERMANENTLY with all contents? press Delete again",
        "удалить НАВСЕГДА? ещё раз Delete" => "delete PERMANENTLY? press Delete again",
        // подписи закладок по умолчанию
        "корень" => "root",
        "конфиг" => "config",
        "поколения" => "generations",
        "программы" => "programs",
        "корзина" => "trash",

        // ── установщик ─────────────────────────────────────────────────────────────────────
        "Куда поставить систему" => "Where to install the system",
        "ставлю…" => "installing…",
        "Готово" => "Done",
        "Не вышло" => "Failed",
        "SATA-дисков не найдено. Ставить некуда." => "No SATA disks found. Nowhere to install.",
        " · с него работает система" => " · the running system boots from it",
        " · здесь уже есть VOID" => " · already has VOID on it",
        "диск {}" => "disk {}",
        "Диск будет стёрт целиком: таблица разделов, все разделы, все файлы. Отменить это нельзя."
            => "The disk will be erased entirely: partition table, every partition, every file. \
                This cannot be undone.",
        "Стереть и поставить" => "Erase and install",
        "На этот диск поставить нельзя: с него работает система прямо сейчас."
            => "This disk cannot be used: the running system boots from it right now.",
        "VOID установлен. Выключи машину, вынь носитель и включи снова — система поднимется с \
         диска. Первая загрузка сама посеет конфиг."
            => "VOID is installed. Power the machine off, remove the medium and switch it on \
                again — the system will come up from the disk. The first boot seeds the config \
                by itself.",
        "Установка не состоялась. Причину ядро сказало в журнал: `klog`. Чаще всего это \
         отсутствие образа установки — то есть загрузка не с носителя."
            => "The installation did not happen. The kernel said why in the log: `klog`. Most \
                often the install image is missing — that is, this is not a boot from the medium.",
        "Установка VOID" => "Install VOID",

        // ── просмотрщик корней store ───────────────────────────────────────────────────────
        "поиск по имени корня" => "search by root name",
        "размер" => "size",
        "частей: {}" => "{} parts",
        "дерево" => "tree",
        "первые байты" => "first bytes",
        "начало содержимого" => "beginning of contents",
        "объект не читается" => "the object does not read",
        "ничего не найдено" => "nothing found",
        "корней в store: {}  ·  Ctrl+C — скопировать имя, мышью — перетащить"
            => "roots in store: {}  ·  Ctrl+C copies the name, the mouse drags it",
        "{} из {} корней" => "{} of {} roots",
        "не вышло скопировать" => "could not copy",
        "корни store" => "store roots",

        // ── диспетчер задач ────────────────────────────────────────────────────────────────
        "производительность" => "performance",
        "приложения" => "applications",
        "службы" => "services",
        "отнято: P{} слот {}" => "revoked: P{} slot {}",
        "отзыв не удался" => "the revocation failed",
        "сети нет: у диспетчера нет канала к серверу"
            => "no network: the manager has no channel to the server",
        "сервер сети не ответил" => "the network server did not answer",
        "сеть включена: сервер снова обслуживает"
            => "network on: the server is serving again",
        "сеть выключена: сервер не обслуживает никого"
            => "network off: the server serves nobody",
        "служб" => "services",
        "приложений" => "applications",
        "чтение+управление" => "read+control",
        "только чтение" => "read only",
        "сеть работает" => "network is up",
        "сеть выключена" => "network is off",
        "сети нет" => "no network",
        "имя" => "name",
        "ЦП" => "CPU",
        "куча" => "heap",
        "состояние" => "state",
        "процесс не выбран" => "no process selected",
        "права процесса {} (P{})" => "rights of {} (P{})",
        "слот" => "slot",
        "права" => "rights",
        "цель" => "target",
        "прав нет" => "no rights",
        "процесс не может ничего вне себя" => "the process can do nothing outside itself",
        "происхождение" => "origin",
        "служба (init)" => "service (init)",
        "content-id образа" => "image content-id",
        "образ из пакета Linux" => "image from a Linux package",
        "образ без хэша" => "image without a hash",
        "{}, последняя минута" => "{}, last minute",
        "замеров ещё нет" => "no measurements yet",
        "загрузка" => "load",
        "архитектура" => "architecture",
        "время работы" => "uptime",
        "из них простой" => "of that, idle",
        "процессов" => "processes",
        ", поднято {}" => ", {} up",
        " (работает {})" => " ({} running)",
        "ядер" => "cores",
        "{} МиБ" => "{} MiB",
        "всего" => "total",
        "занято" => "in use",
        "свободно" => "free",
        "занято, доля" => "in use, share",
        "страница" => "page",
        "4 КиБ" => "4 KiB",
        "подкачки" => "swap",
        "нет (и не будет)" => "none (and never will be)",
        "{} из {} МиБ" => "{} of {} MiB",
        "нет права" => "no right",
        "нет права обзора процессов" => "no process-overview right",
        "никаких" => "none",
        // устройства вкладки «производительность»
        "процессор" => "processor",
        "память" => "memory",
        "диск" => "disk",
        "сеть" => "network",
        "видео" => "video",
        "датчики" => "sensors",
        "счётчиков нет" => "no counters",
        "нет драйверов" => "no drivers",
        // что значит право
        "объекты системы:" => "system objects:",
        "класть и читать по" => "put and read by",
        "content-id" => "content-id",
        "именованный корень —" => "a named root —",
        "вход в поколение" => "the way into a generation",
        "или файл" => "or a file",
        "одно значение в store" => "one value in the store",
        "канал к процессу:" => "a channel to a process:",
        "можно звать его и" => "you may call it and",
        "просить за себя" => "ask on your behalf",
        "ответить на один" => "answer exactly one",
        "вызов; живёт до" => "call; lives until",
        "ответа" => "the answer",
        "диск целиком: секторы" => "the whole disk: sectors",
        "мимо store" => "bypassing the store",
        "сетевая карта: кадры" => "the network card: frames",
        "мимо служб" => "bypassing the services",
        "регистры устройства —" => "device registers —",
        "прямое управление" => "direct control of",
        "железом" => "the hardware",
        "память для устройства:" => "memory for a device:",
        "оно пишет в неё само" => "it writes there itself",
        "выключить машину" => "power the machine off",
        "общая память с другим" => "memory shared with another",
        "процессом (так ездят" => "process (this is how",
        "кадры окон)" => "window frames travel)",
        "прерывания устройства:" => "device interrupts:",
        "спать до сигнала" => "sleep until signalled",
        "видеть процессы и их" => "see processes and their",
        "права, а с `w` —" => "rights, and with `w` —",
        "отзывать их" => "revoke them",
        "вид неизвестен этой" => "this build does not know",
        "сборке" => "this kind",
        // объяснение «прав обзора нет»
        "Диспетчер показывает не всю систему," => "The manager does not show the whole system,",
        "а ровно то, что выдано ему самому." => "only exactly what was handed to it.",
        "Права `sysview` у него нет — поэтому" => "It has no `sysview` right — that is why",
        "списки пусты. Это не значит, что" => "the lists are empty. That does not mean",
        "никого нет: это значит, что смотреть" => "nobody is there: it means looking",
        "не дано." => "was not granted.",
        "Выдать — в конфиге поколения:" => "To grant it, in the generation config:",
        "     право обзора композитору," => "     overview right to the compositor,",
        "     детям НЕ наследуется" => "     NOT inherited by children",
        "     кому он отдаёт его по просьбе" => "     whom it hands it to on request",
        "Затем `rebuild` и перезагрузка." => "Then `rebuild` and reboot.",
        "Без второй строки не отдаст никому —" => "Without the second line it grants nobody —",
        "это правильное состояние по умолчанию." => "which is the right default.",

        // ── композитор ─────────────────────────────────────────────────────────────────────
        "композитор" => "compositor",
        "окну не хватило места" => "the window ran out of room",
        "кончилось адресное пространство под кадры окон — закройте часть окон"
            => "the address space for window frames is exhausted — close some windows",
        "ЗАПУСКАЕТСЯ" => "STARTING",
        "дольше обычного — окна всё нет" => "longer than usual — still no window",
        "НЕ ЗАПУСТИЛОСЬ" => "DID NOT START",
        "ЗАВЕРШИЛОСЬ БЕЗ ОКНА" => "EXITED WITHOUT A WINDOW",
        "код выхода {}" => "exit code {}",
        "вывода не было" => "there was no output",
        "Super+Q — убрать" => "Super+Q to dismiss",

        // ── окно «добро пожаловать» ────────────────────────────────────────────────────────
        "Добро пожаловать" => "Welcome",
        "Операционная система без root и без пользователей: всё, что программа может сделать, \
         лежит у неё в руках отдельными правами — и эти права видно."
            => "An operating system with no root and no users: everything a program is able to \
                do it holds in its hands as separate rights — and those rights are visible.",
        "Строка запуска" => "Launcher",
        "Терминал" => "Terminal",
        "Обзор столов" => "Desktop overview",
        "Чем VOID отличается" => "What makes VOID different",
        "Нет root и нет пользователей." => "No root and no users.",
        "Система — это конфиг." => "The system is its config.",
        "Ядро микро. Композитор, драйверы, шелл — обычные программы."
            => "The kernel is micro. Compositor, drivers, shell are ordinary programs.",
        "Как это выключить" => "How to turn this off",
        "Это окно — обычная программа. Открылось оно потому, что названо в конфиге:"
            => "This window is an ordinary program. It opened because the config names it:",
        "Убери имя из списка, скажи rebuild — и окна больше не будет. Так меняется всё \
         остальное: gens покажет поколения, switch вернёт прежнее."
            => "Remove the name from the list, say rebuild — and the window is gone. Everything \
                else changes the same way: gens lists the generations, switch goes back.",

        // ── терминал ───────────────────────────────────────────────────────────────────────
        "терминал" => "terminal",
        "панель" => "pane",
        "живая" => "alive",
        "мертва" => "dead",
        "команды панелей" => "pane commands",
        "ПАНЕЛИ:" => "PANES:",
        "разбить" => "split",
        "поперёк" => "across",
        "закрыть" => "close",
        "выход" => "quit",
        "переход" => "go to",
        "уроненное уже забрали" => "the dropped item was already taken",
        "[процесс завершился — Ctrl-A x закрыть панель]"
            => "[the process exited — Ctrl-A x closes the pane]",
        "[не удалось запустить шелл]" => "[could not start the shell]",

        // ── шелл: баннер и справка ─────────────────────────────────────────────────────────
        " — шелл VOID (ADR 0006/0013). `\\выражение` — вычислить, иначе команда. `help` — команды, `exit` — назад в vsh.\n"
            => " — the VOID shell (ADR 0006/0013). A line starting with `\\` is an expression, \
                anything else is a command. `help` lists them, `exit` goes back to vsh.\n",
        "окно" => "window",
        " — шелл VOID. Строка с ведущим `\\` — выражение, иначе команда.\n"
            => " — the VOID shell. A line starting with `\\` is an expression, anything else is \
                a command.\n",
        "  Выражение — с ведущим \\: \\x = 5 · \\ping(\"10.0.2.2\")\n"
            => "  Expression — lead with \\: \\x = 5 · \\ping(\"10.0.2.2\")\n",
        "  Язык: x = 5 · |a| a + 1 · if c { a } else { b } · [1, 2] · map(f, L)\n"
            => "  Language: x = 5 · |a| a + 1 · if c { a } else { b } · [1, 2] · map(f, L)\n",
        "  Конвейер: \\ls() |> grep(\"vv\") |> count()\n"
            => "  Pipeline: \\ls() |> grep(\"vv\") |> count()\n",
        "список файлов (каталог или текущий)" => "list files (a folder, or the current one)",
        "показать содержимое (cat A > B — записать содержимым)"
            => "show contents (cat A > B writes them into B)",
        "последние ~32 байта файла" => "the last ~32 bytes of a file",
        "сменить каталог (.. вверх, без арг — в корень)"
            => "change folder (.. goes up, no argument goes to the root)",
        "текущий каталог" => "the current folder",
        "вид, размер и время последнего изменения" => "kind, size and time of last change",
        "удалить файл или пустой каталог (rm \"-r\" — с содержимым)"
            => "delete a file or an empty folder (rm \"-r\" takes the contents too)",
        "переименовать/переместить файл или каталог" => "rename or move a file or a folder",
        "копировать файл или каталог (мгновенно: то же содержимое)"
            => "copy a file or a folder (instant: the very same contents)",
        "напечатать ($x — переменная; TEXT > FILE — запись)"
            => "print ($x is a variable; TEXT > FILE writes it)",
        "экранный редактор: ^S сохранить, ^Q выход (программа)"
            => "full-screen editor: ^S saves, ^Q quits (a program)",
        "фильтр строк списка (для конвейеров)" => "filter list lines (for pipelines)",
        "запустить программу из store (или просто NAME)"
            => "run a program from the store (or just NAME)",
        "разморозить процесс из образа" => "thaw a process from its image",
        "ICMP-пинг адреса A.B.C.D" => "ICMP ping of A.B.C.D",
        "DNS: имя → адрес (возвращает строку)" => "DNS: name → address (returns a string)",
        "открыть TCP → хэндл (+ tcp-send/recv/close)"
            => "open TCP → handle (+ tcp-send/recv/close)",
        "скачать по HTTP потоком в store (корень R)"
            => "download over HTTP straight into the store (root R)",
        "сводка/кусок скачанного (см. fetch)" => "summary or a chunk of a download (see fetch)",
        "отвязать сырой корень store" => "unbind a raw store root",
        "сырые корни store (bin/*, system/*, …)" => "raw store roots (bin/*, system/*, …)",
        "посеять /etc/system/*.vv" => "seed /etc/system/*.vv",
        "пакеты nixpkgs: install/list/remove/rollback/gc (программа)"
            => "nixpkgs packages: install/list/remove/rollback/gc (a program)",
        "собрать поколение из /etc/system/*.vv" => "build a generation from /etc/system/*.vv",
        "показать поколения системы (активно — *)"
            => "list the system generations (* marks the active one)",
        "выбрать поколение (после ребута)" => "choose a generation (takes effect after reboot)",
        "задать поколение из файла-конфига" => "set the generation from a config file",
        "сказать человеку: всплывашка и колокольчик в панели"
            => "tell the human: a popup and the bell in the panel",
        "текущее время системы (UTC)" => "the current system time (UTC)",
        "N случайных байт от ядра (hex)" => "N random bytes from the kernel (hex)",
        "подробный трейс ядра ([ipc]/[obj]/…)" => "verbose kernel trace ([ipc]/[obj]/…)",
        "очистить экран" => "clear the screen",
        "эта справка" => "this help",
        "выйти в vsh (спасательный шелл)" => "leave to vsh (the rescue shell)",
        "замер: сколько store принимает за сессию (МиБ)"
            => "measurement: how much the store takes in one session (MiB)",
        "разложить NAR из корня store в файлы" => "unpack a NAR from a store root into files",

        // ── заголовки окон ─────────────────────────────────────────────────────────────────
        "Файлы" => "Files",
        "Диспетчер задач" => "Task manager",
        "обои" => "wallpaper",

        // состояния процессов и права словами
        "готов" => "ready",
        "ждёт IPC" => "waits for IPC",
        "ждёт ответа" => "waits for a reply",
        "ждёт ребёнка" => "waits for a child",
        "завершён" => "exited",
        "ждёт ввода" => "waits for input",
        "ждёт futex" => "waits on futex",
        "ждёт IRQ" => "waits for IRQ",
        "спит" => "sleeps",
        "ждёт трубы" => "waits on a pipe",
        "ждёт ребёнка" => "waits for a child",
        "неизвестно" => "unknown",
        "чтение" => "read",
        "запись" => "write",
        "запуск" => "exec",
        "отправка" => "send",
        "передача" => "grant",
        "{} КиБ" => "{} KiB",

        _ => s,
    }
}
