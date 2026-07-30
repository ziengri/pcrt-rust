# PCRT Rust

Новая реализация BusPCRT развивается независимо от действующей Python-системы.
До явного этапа миграции Python-службы остаются production-версией.

## Начальная структура

```text
rust/
  crates/             # библиотеки с изолированной ответственностью
  bins/               # будущие бинарники systemd-служб
  contracts/          # версионируемые внешние контракты и fixtures
  docs/               # архитектурные решения и эксплуатационные требования
  integration-tests/  # тесты на границах процессов и внешних систем
```

Первые три crate намеренно не зависят от инфраструктурных библиотек. Они задают
типы предметной области, порядок конфигурации и единый механизм graceful shutdown.
Следующий рабочий crate - `pcrt-result-queue`, независимая SQLite-очередь готовых
результатов для uploader.

## Команды

```bash
cd rust
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all --check
```

## Video encoding experiment

`pcrt-video-reencode` sends decoded BGR frames through an `ffmpeg` rawvideo pipe
and encodes them with the same FFV1/Matroska settings as the Python recorder:

```bash
cargo run -p pcrt-video-reencode --release -- \
  --input /path/to/input.mp4 \
  --output /tmp/reencoded.mkv \
  --width 256 --height 256 --fps 25
```

It refuses to overwrite an existing output file.

For H.264 with fast preset and CRF 18:

```bash
cargo run -p pcrt-video-reencode --release -- \
  --input /path/to/input.mp4 \
  --output /tmp/reencoded-h264.mkv \
  --codec libx264 --preset fast --crf 18
```

## Принципы

- Внешние контракты версионируются до реализации сервиса.
- Один компонент отвечает за одно хранилище или транспортный контракт.
- Сессия является единицей атомарности, восстановления и идемпотентности.
- Результат может быть доставлен повторно; API дедуплицирует его по `session_id`.
- Секреты не хранятся в репозитории, CLI-аргументах или логах.
- Каждый демон обязан корректно обработать `SIGTERM` и восстановиться после сбоя.

Подробности: [архитектура](docs/architecture.md), [план улучшений](docs/improvements.md)
и модули [очередь результатов](docs/modules/pcrt-result-queue.md),
[API-клиент](docs/modules/pcrt-api-client.md),
[storage](docs/modules/pcrt-storage.md),
[door](docs/modules/pcrt-door.md),
[uploader](docs/modules/pcrt-uploader.md).
