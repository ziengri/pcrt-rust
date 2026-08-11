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

## Service Configuration

The project-local Rust service configuration lives beside the workspace:

| File | Reader |
| --- | --- |
| `config.env` | Shared sessions root, result queue and ZeroMQ endpoint. |
| `door_gateway.env` | Native RS-232 door gateway. |
| `recorder-cam.env` through `recorder-cam4.env` | One native recorder per camera/door. |
| `processor.env` | Native OpenCV/OpenVINO processing. |
| `uploader.env` | Native result delivery and API credentials. |

Run a binary from `rust/` to use these default filenames, or pass explicit
`--config-env-file` and `--env-file` paths. Every recorder, processor and
uploader must resolve the same `SESSIONS_DIR` and `RESULT_QUEUE_DB` from
`config.env`.

`external-config/device.env` is the template for the only device-local runtime
file. Install its populated copy as `/etc/pcrt/device.env`; it contains `BUS_ID`
and `NUMBER_CAMS`, which are read by the processor and door gateway.

The full file map and native launch commands are in
[configuration](docs/configuration.md).

## Принципы

- Внешние контракты версионируются до реализации сервиса.
- Один компонент отвечает за одно хранилище или транспортный контракт.
- Сессия является единицей атомарности, восстановления и идемпотентности.
- Результат может быть доставлен повторно; API дедуплицирует его по `session_id`.
- Секреты не передаются в CLI-аргументах и не выводятся в логи.
- Каждый демон обязан корректно обработать `SIGTERM` и восстановиться после сбоя.

Подробности: [архитектура](docs/architecture.md), [план улучшений](docs/improvements.md)
и модули [очередь результатов](docs/modules/pcrt-result-queue.md),
[API-клиент](docs/modules/pcrt-api-client.md),
[storage](docs/modules/pcrt-storage.md),
[door](docs/modules/pcrt-door.md),
[uploader](docs/modules/pcrt-uploader.md).
