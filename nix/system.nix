# system.nix — Веха 40: декларативная конфигурация VOID, вычисляемая НАСТОЯЩИМ Nix на ХОСТЕ.
#
# Ровно модель NixOS: язык Nix — это ВЫЧИСЛИТЕЛЬ, он работает на хосте и производит конкретный
# результат; работающая система Nix не исполняет — читает уже-вычисленное. Здесь Nix вычисляет
# СПИСОК сервисов в текст-конфиг, который читает декларативный init VOID (kernel/src/init.rs).
#
# Собрать поколение и доставить в store:
#   nix eval --raw --file nix/system.nix > /tmp/gen3.conf
#   tools/.../void-store-import void-disk.img put /tmp/gen3.conf system/gen3
#   # в vsh: switch gen3, затем перезагрузка
#
# Здесь конфиг задан декларативно списком записей {kind, name, caps}; поменяй список —
# получишь другое поколение. Это и есть «configuration.nix родными средствами».
let
  # Одна запись конфига → строка формата init: "<kind> <name> <cap> <cap> …".
  mkLine = e:
    "${e.kind} ${e.name}"
    + (if e.caps == [] then "" else " " + builtins.concatStringsSep " " e.caps);

  # Декларативное описание системы (полное поколение: файлы + сеть + интерактивный shell).
  services = [
    { kind = "service"; name = "posixfs"; caps = [ "store:rw" ]; }
    { kind = "service"; name = "net-srv"; caps = [ "dev:net:rw" ]; }
    { kind = "shell"; name = "vsh"; caps = [ "endpoint:posixfs" "store:xw" "endpoint:net-srv" "env" ]; }
  ];
in
''
# VOID system — сгенерировано nix/system.nix (вычислил настоящий Nix на хосте)
''
+ builtins.concatStringsSep "\n" (map mkLine services)
+ "\n"
