# conchy-session

[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.80%2B-orange.svg)](https://www.rust-lang.org/)
[![Platform](https://img.shields.io/badge/Platform-Linux%20(Void%20%2F%20Arch%20%2F%20Wayland)-green.svg)]()

**`conchy-session`** — лёгкий, надёжный и быстрый оркестратор пользовательской сессии Wayland и супервизор процессов для **Conchy OS** (модель *rolling release*: без фиксированных версий и нумерованных релизов, пакеты собираются из `main` по дате) и **Void Linux** (вторичная цель — **Arch Linux**).

Разработан для полностью независимой работы от `systemd --user` в окружениях на базе `runit` или `seatd`. Обеспечивает подготовку окружения сессии, фазовый запуск композитора и сервисов с автоматическим перезапуском, синхронизацию с D-Bus, обработку XDG Autostart и чистое завершение дочерних процессов без зомби.

---

## 🌟 Ключевые возможности

1. **Полная независимость от `systemd` (`runit` / `seatd` ready)**:
   - Не требует `systemd --user`, `logind` или `systemctl import-environment`.
   - Проверяет и инициализирует `XDG_RUNTIME_DIR` (существующий каталог с правами `0700`, `/run/user/<uid>` или резервный `/tmp/conchy-runtime-<uid>` с правами `0700`).
   - Включает режим Linux Subreaper (`prctl(PR_SET_CHILD_SUBREAPER, 1)`): отсоединившиеся дочерние демоны и приложения переподчиняются процессу `conchy-session`, гарантируя сбор зомби-процессов.
   - Переводит процесс в изолированную группу процессов (`setpgid(0, 0)`).

2. **Переменные окружения и D-Bus Activation Environment Sync**:
   - Устанавливает стандартные переменные Wayland-окружения (`XDG_CURRENT_DESKTOP=conchy`, `XDG_SESSION_DESKTOP=conchy`, `XDG_SESSION_TYPE=wayland`, `DESKTOP_SESSION=conchy`, `QT_QPA_PLATFORM=wayland;xcb`, `QT_WAYLAND_DISABLE_WINDOWDECORATION=1`, `GDK_BACKEND=wayland,x11,*`, `SDL_VIDEODRIVER=wayland`, `CLUTTER_BACKEND=wayland`, `MOZ_ENABLE_WAYLAND=1`, `ELECTRON_OZONE_PLATFORM_HINT=auto`, `_JAVA_AWT_WM_NONREPARENTING=1`) и пользовательские переопределения из секции `env`.
   - Проверяет `DBUS_SESSION_BUS_ADDRESS` и `$XDG_RUNTIME_DIR/bus`, при необходимости запускает `dbus-daemon --session --fork --print-address`.
   - После готовности сокета Wayland экспортирует ключевые переменные сессии (`XDG_CURRENT_DESKTOP`, `XDG_SESSION_DESKTOP`, `XDG_SESSION_TYPE`, `DESKTOP_SESSION`, `WAYLAND_DISPLAY`, `DISPLAY`, `XDG_RUNTIME_DIR`, `QT_QPA_PLATFORM`, `GDK_BACKEND`, `MOZ_ENABLE_WAYLAND`, `ELECTRON_OZONE_PLATFORM_HINT`) в окружение активации D-Bus через `org.freedesktop.DBus.UpdateActivationEnvironment`.
   - Обеспечивает корректную работу `xdg-desktop-portal`, polkit-агентов, браузеров и Flatpak-приложений.

3. **Оркестрация и супервизия сервисов**:
   - **Фазовый запуск**:
     - *Pre-Wayland* (`wait-for-wayland false`): запуск базовых фоновых служб до композитора (по умолчанию `pipewire`, `wireplumber`, `conchy-config daemon`).
     - *Compositor*: запуск `conchy-wm` и ожидание готовности сокета `wayland-N` в `$XDG_RUNTIME_DIR` (таймаут задаётся через `socket-wait-timeout`, по умолчанию `10` с).
     - *Post-Wayland* (`wait-for-wayland true`): запуск оболочки (`conchy-shell daemon`) и дополнительных пользовательских сервисов (polkit-агенты, демон уведомлений и др.) сразу после готовности Wayland-сокета и синхронизации D-Bus.
   - **Политики перезапуска (`RestartPolicy`)**:
     - `always`, `on-failure`, `never`.
     - Защита от бесконечного цикла падений (*Crash loop protection*): в конфигурации `session.kdl` действуют встроенные лимиты — до `10` перезапусков в окне `60` с с задержкой `1000` мс (в legacy-формате `session.toml` эти параметры задаются явно).
   - **Журналирование без `journald`**:
     - Журнал самого оркестратора (старт/остановка служб, причины завершения с кодом/сигналом, счётчики перезапусков и срабатывание защиты от цикла падений) пишется в `$XDG_STATE_HOME/conchy/logs/session.log` (по умолчанию `~/.local/state/conchy/logs/session.log`) с ротацией по размеру (до 3 архивов `.1`–`.3` по 2 МиБ).
     - `stdout` и `stderr` композитора и каждой управляемой службы перенаправляются в `<служба>.log` в том же каталоге с отметками `[conchy-session]` о запусках и падениях.

4. **Поддержка спецификации XDG Autostart**:
   - Сканирует `$XDG_CONFIG_HOME/autostart` (иначе `~/.config/autostart`), `$XDG_CONFIG_DIRS/autostart` и `/etc/xdg/autostart` с перекрытием одноимённых файлов по приоритету каталогов.
   - Поддерживает `Type=Application`, `Hidden=true`, `OnlyShowIn` (учитывает `conchy` и `wlroots`), `NotShowIn` (исключает `conchy`), `TryExec`, а также фильтрацию полевых макросов вроде `%u` и `%f`.

5. **Изящное и чистое завершение (`Graceful Teardown`)**:
   - Завершение композитора (`conchy-wm`) или сигналы `SIGTERM`, `SIGINT`, `SIGHUP` автоматически инициируют выход из сессии.
   - Отправляет `SIGTERM` всем отслеживаемым дочерним процессам с ожиданием до `3` секунд перед `SIGKILL` (`2` секунды при таймауте запуска композитора).
   - Собирает всех зомби через `waitpid(-1, ..., WNOHANG)`.

---

## 🚀 Быстрый старт и CLI

### Сборка

`Cargo.toml` подключает библиотеку схем `conchy-config` как локальную зависимость `{ path = "../config" }` (в рабочем пространстве Conchy OS рядом с `conchy-session` используется каталог или симлинк `../config -> ../conchy-config`):

```bash
cargo build --release
```

Бинарный файл: `target/release/conchy-session`.

### Запуск сессии

> Запускайте `conchy-session` только из дисплейного менеджера или с чистого TTY (не запускайте внутри уже работающей графической сессии).

```bash
conchy-session
```

С переопределением команды запуска композитора:

```bash
conchy-session --compositor /usr/bin/conchy-wm
# или с аргументами:
conchy-session --compositor "conchy-wm --verbose"
```

### Проверка конфигурации (Dry-Run)

```bash
conchy-session --dry-run
```

### Просмотр и отслеживание логов

На Void Linux нет `journald`, поэтому `conchy-session` пишет журнал оркестратора в `$XDG_STATE_HOME/conchy/logs/session.log` (по умолчанию `~/.local/state/conchy/logs/session.log`), а `stdout` и `stderr` композитора и каждой службы — в `<служба>.log` в том же каталоге (ротация по размеру 2 МиБ, до 3 архивов `.1`–`.3`):

```bash
# Список доступных логов
conchy-session logs

# Показать лог службы или самой сессии
conchy-session logs conchy-shell
conchy-session logs session

# Следить за логом в реальном времени (аналог tail -f, переживает ротацию)
conchy-session logs conchy-shell -f

# Последние 100 строк с учётом ротированных архивов
conchy-session logs conchy-shell --all -n 100
```

### Параметры командной строки

- `logs [SERVICE]` — показать или отслеживать (`-f`/`--follow`, `-n`/`--lines`, `-a`/`--all`, `--list`) логи сессии (`session`) или конкретной службы.
- `-c, --config <FILE>` — явный путь к файлу конфигурации (`.kdl` разбирается как KDL, любой другой файл — как legacy TOML; по умолчанию используется поиск `session.kdl` с откатом к `session.toml`).
- `--compositor <COMMAND>` — переопределить команду запуска композитора (при наличии пробелов разбивается на исполняемый файл и аргументы).
- `--no-autostart` — отключить запуск приложений из XDG Autostart (`~/.config/autostart`, `/etc/xdg/autostart`).
- `--dry-run` — проверить конфигурацию, `XDG_RUNTIME_DIR`, наличие бинарников в `PATH` и список автозапуска без старта сессии.
- `--dump-default-config` — вывести встроенную конфигурацию по умолчанию (KDL) в `stdout` и выйти.
- `-l, --log-level <LOG_LEVEL>` — уровень логирования (`trace`, `debug`, `info`, `warn`, `error`; также переменная `CONCHY_LOG`, по умолчанию `info`).
- `-h, --help`, `-V, --version` — справка и метаданные пакета.

---

## ⚙️ Конфигурация (`session.kdl`)

Основной формат конфигурации — **KDL** (`session.kdl`), схема которого определена в `conchy-config`. Старый формат `session.toml` (`resources/session.toml`) оставлен исключительно как **legacy fallback** для обратной совместимости и больше не рекомендуется для новых установок.

Файл конфигурации ищется в следующем порядке:
1. Явный путь, переданный через `-c` / `--config <FILE>`
2. `$XDG_CONFIG_HOME/conchy/session.kdl` (legacy fallback: `$XDG_CONFIG_HOME/conchy/session.toml`)
3. `~/.config/conchy/session.kdl` (legacy fallback: `~/.config/conchy/session.toml`)
4. `/etc/conchy/session.kdl` (legacy fallback: `/etc/conchy/session.toml`)
5. Встроенная KDL-конфигурация по умолчанию

Вывести дефолтную KDL-конфигурацию:

```bash
mkdir -p ~/.config/conchy
conchy-session --dump-default-config > ~/.config/conchy/session.kdl
```

Пример актуальной конфигурации по умолчанию (совпадает с выводом `conchy-session --dump-default-config` и [`resources/session.kdl`](resources/session.kdl)):

```kdl
// ==============================================================================
// Conchy OS - Wayland User Session Configuration
// Location: ~/.config/conchy/session.kdl or /etc/conchy/session.kdl
// ==============================================================================

session {
    socket-wait-timeout 10
    autostart true
}

env {
    XCURSOR_SIZE "24"
}

compositor "conchy-wm"

// ==============================================================================
// Supervised Services (restarted automatically on crash)
// ==============================================================================

service "pipewire" {
    command "pipewire"
    restart "always"
    wait-for-wayland false
}

service "wireplumber" {
    command "wireplumber"
    restart "always"
    wait-for-wayland false
}

service "conchy-config" {
    command "conchy-config"
    args "daemon"
    restart "always"
    wait-for-wayland false
}

service "conchy-shell" {
    command "conchy-shell"
    args "daemon"
    restart "always"
    wait-for-wayland true
}
```

### Основные секции `session.kdl`

- `session`: `socket-wait-timeout` (секунды ожидания Wayland-сокета) и `autostart` (`true`/`false`).
- `env`: дополнительные переменные окружения сессии (применяются поверх стандартных переменных Wayland и `XDG_CURRENT_DESKTOP=conchy`).
- `compositor`: имя или путь исполняемого файла композитора.
- `service "<name>"`: `command`, `args` (один или несколько строковых аргументов), `restart` (`"always"`, `"on-failure"`, `"never"`), `wait-for-wayland` (`false` — запуск до композитора, `true` — после готовности сокета Wayland).

---

## 🖥 Интеграция с дисплейными менеджерами

Скопируйте [`resources/conchy.desktop`](resources/conchy.desktop) в `/usr/share/wayland-sessions/conchy.desktop`:

```ini
[Desktop Entry]
Name=Conchy
Comment=Lightweight scrolling Wayland desktop environment for Conchy OS
Exec=conchy-session
Type=Application
DesktopNames=conchy
Keywords=wayland;compositor;scrolling;session;conchy;
```

Значение `DesktopNames=conchy` соответствует `XDG_CURRENT_DESKTOP=conchy` в `src/env.rs` и фильтру `OnlyShowIn` / `NotShowIn` в `src/autostart.rs`.

---

## 📄 Лицензия

GPL-3.0-or-later (см. [`LICENSE`](LICENSE)).
