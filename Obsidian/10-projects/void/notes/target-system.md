---
title: Целевая система — что VOID в итоге заменяет
created: 2026-07-01
tags: [project/void, topic/goal, topic/nixos, topic/context]
status: active
---

# Целевая система — что VOID заменяет

Изучил реальное рабочее устройство пользователя (он предложил — «для общего понимания»). Это
**переносной зашифрованный NixOS на съёмном SSD**, который он подключает к любому ПК, чтобы «всё было
везде, где есть компьютер». **Это буквально тезис VOID** ([[0002-persistent-content-addressed-capability-core]]),
только вручную выстроенный поверх неподходящего фундамента — ровно та мотивация, из-за которой VOID
существует.

## Профиль

- **NixOS 26.11 (unstable)**, флейк + **home-manager**, конфиг — git-репо `~/Project/Nix/nixos`
  (модульно: `system/` `desktop/` `apps/` `shell/`). «A setup for mildly paranoid people».
- **Носитель:** съёмный SSD (SPCC) — `/boot` (vfat) + **LUKS-ext4 root**; **lanzaboote** (secure boot).
  Грузится на любой машине.
- **GPU: amdgpu** (реальное железо) → подтверждает выбор ADR 0007 «целимся на AMD».
- **Рабочий стол:** Wayland — **niri** + **noctalia-shell**, `greetd`, gnome как fallback (селектор).
- **Шелл: bash** (с темизированным промптом) → подтверждает мотивацию vvsh ([[0006-vvsh-lisp-config-shell]]).
- **Свои Rust-инструменты** как флейк-инпуты: **spacer** (файловый менеджер), **void-connect**, **rust-rim**
  (`the-void-fox`). Пользователь пишет свой userland на Rust — совпадает с native-Rust-историей VOID.
- Приложения: librewolf, claude-code, flatpak.

## Почему это важно для роадмапа

- **`portable.nix` = проблема «длинного хвоста железа», в лоб.** Чтобы зашифрованный корень
  смонтировался на ЛЮБОМ хосте, в initrd впихнуты ВСЕ драйверы (xhci/ehci/ohci/uhci, usb-storage/uas,
  usbhid, atkbd/i8042, ahci+все sata, nvme, sdhci/rtsx, virtio…). **Ровно поэтому важен хостинг
  неизменённых Linux-драйверов** ([[lx-linux]], dde_linux): VOID должен подниматься на произвольном
  чипсете хоста. e1000 доказал механизм ([[lx-e1000-rx]]); настоящий приз — host-agnostic boot.
- **Персистентность/переносимость** у пользователя — ручная (LUKS-SSD + всё-в-initrd). VOID даёт это
  из ядра (single-level store, «всё переживает ребут»). Портативный GUI ([[0007-graphics-native-compositor]])
  — продолжение той же идеи в графику.
- **Что VOID должен догнать, чтобы заменить этот сетап:** (1) грузиться на разном железе (драйверы по
  требованию — NIC/USB/SD/NVMe/GPU); (2) конфиг-язык уровня flake+home-manager (**vvsh**, ADR 0006);
  (3) шелл вместо bash (**vvsh**); (4) графическая оболочка уровня niri+noctalia (**ADR 0007**, свой
  стек, тайлинг как niri, AMD-ускорение); (5) свои Rust-приложения (файловый менеджер, браузер — гора,
  и т.д.). Это многолетняя дорога — но цель конкретна и теперь записана.

## Связано
- [[0002-persistent-content-addressed-capability-core]] · [[0006-vvsh-lisp-config-shell]] ·
  [[0007-graphics-native-compositor]] · [[lx-linux]] · [[known-gaps]] · [[todo]]
