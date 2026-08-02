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
[door ZMQ](docs/modules/pcrt-door-zmq.md),
[uploader](docs/modules/pcrt-uploader.md).
