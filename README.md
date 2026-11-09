# conchy-session

[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.80%2B-orange.svg)](https://www.rust-lang.org/)
[![Platform](https://img.shields.io/badge/Platform-Linux%20(Void%20%2F%20Wayland)-green.svg)]()

**`conchy-session`** — лёгкий, надёжный и быстрый оркестратор пользовательской сессии Wayland и супервизор процессов для **Conchy OS** и **Void Linux**.

Разработан специально для независимой работы от `systemd --user` в окружениях на базе `runit` или `seatd`. Обеспечивает полноценный запуск рабочего стола, автоматический перезапуск упавших сервисов, интеграцию с D-Bus, обработку автозапуска XDG и атомарное завершение всех процессов сессии без зомби.

---

## 🌟 Ключевые возможности

1. **Полная независимость от systemd (`runit` / `seatd` ready)**:
   - Не требует `systemd --user`, `logind` или `systemctl import-environment`.
   - Включает режим Linux Subreaper (`PR_SET_CHILD_SUBREAPER`): все отсоединившиеся дочерние демоны и приложения автоматически реперентятся на `conchy-session`, гарантируя сбор зомби-процессов.
   - Изолированная группа процессов (`setpgid`): гарантирует мгновенный и чистый выход (`killpg`) без зависших фоновых процессов.

2. **D-Bus Activation Environment Sync**:
   - Автоматически проверяет наличие и при необходимости запускает `dbus-daemon --session`.
   - Экспортирует переменные окружения сессии (`WAYLAND_DISPLAY`, `XDG_CURRENT_DESKTOP=conchy`, `XDG_SESSION_TYPE=wayland`, `QT_QPA_PLATFORM` и др.) в D-Bus activation окружение через вызов `org.freedesktop.DBus.UpdateActivationEnvironment`.
   - Обеспечивает корректную работу `xdg-desktop-portal`, polkit-агентов, браузеров и Flatpak-приложений.

3. **Оркестрация и супервизия сервисов**:
   - **Фазовый запуск**:
     - *Pre-Wayland*: запуск базовых аудио и медиа сервисов (`pipewire`, `wireplumber`).
     - *Compositor*: запуск `conchy-wm` с отслеживанием готовности сокета Wayland в `$XDG_RUNTIME_DIR`.
     - *Post-Wayland*: запуск интерфейса (`conchy-shell`), polkit-агентов и системного трея сразу после готовности сокета.
   - **Политики перезапуска (`RestartPolicy`)**:
     - `always`, `on-failure`, `never`.
     - Защита от бесконечного цикла падений (*Crash loop protection*): отслеживание окна перезапусков (`max_restarts` за `restart_window_secs`).

4. **Поддержка спецификации XDG Autostart**:
   - Сканирует `~/.config/autostart` и `/etc/xdg/autostart`.
   - Поддерживает стандартные директивы: `OnlyShowIn`, `NotShowIn`, `TryExec`, `Hidden=true`, очистку макросов `%u`/`%f`.

5. **Изящное и чистое завершение (`Graceful Teardown`)**:
   - Завершение композитора (`conchy-wm`) автоматически инициирует закрытие сессии.
   - Перехватывает сигналы `SIGTERM`, `SIGINT`, `SIGHUP`.
   - Отправляет `SIGTERM` всем дочерним процессам сессии с таймаутом (3 секунды) перед `SIGKILL`.
   - Собирает всех зомби через `waitpid(..., WNOHANG)`.

---

## 🚀 Быстрый старт

### Сборка
```bash
cargo build --release
```
Бинарный файл: `target/release/conchy-session`.

### Запуск сессии
```bash
conchy-session
```
Либо с указанием пути к кастомному композитору:
```bash
conchy-session --compositor /usr/bin/conchy-wm
```

### Проверка конфигурации (Dry-Run)
```bash
conchy-session --dry-run
```

---

## ⚙️ Конфигурация (`session.toml`)

Файл конфигурации ищется по путям:
1. `$XDG_CONFIG_HOME/conchy/session.toml` (обычно `~/.config/conchy/session.toml`)
2. `/etc/conchy/session.toml`
3. Встроенная конфигурация по умолчанию (fallback)

Вывести дефолтную конфигурацию:
```bash
conchy-session --dump-default-config > ~/.config/conchy/session.toml
```

Пример конфигурации:
```toml
[session]
socket_wait_timeout_secs = 10
autostart = true

[session.env]
XCURSOR_SIZE = "24"

[compositor]
command = "conchy-wm"
args = []

[[services]]
name = "pipewire"
command = "pipewire"
restart = "always"
wait_for_wayland = false

[[services]]
name = "wireplumber"
command = "wireplumber"
restart = "always"
wait_for_wayland = false

[[services]]
name = "conchy-shell"
command = "conchy-shell"
restart = "always"
wait_for_wayland = true
```

---

## 🖥 Интеграция с дисплейными менеджерами

Скопируйте `resources/conchy.desktop` в `/usr/share/wayland-sessions/conchy.desktop`:

```ini
[Desktop Entry]
Name=Conchy
Comment=Lightweight scrolling Wayland desktop environment for Conchy OS
Exec=conchy-session
Type=Application
DesktopNames=conchy
Keywords=wayland;compositor;scrolling;session;conchy;
```

---

## 📄 Лицензия
GPL-3.0-or-later.
