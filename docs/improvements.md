# Улучшения новой версии

Приоритет определён риском потери пассажиропотока, данных или секретов, а не
удобством рефакторинга.

## P0: до первого production-сервиса

| Улучшение | Проблема текущей версии | Решение в Rust |
| --- | --- | --- |
| Session directory | Между перемещениями MKV и meta возможна сирота | Один каталог на сессию, manifest, fsync и атомарный rename |
| Recovery | Cleanup удаляет незавершённые данные | Валидация manifest и recovery при старте |
| Idempotency | Успешный HTTP и падение до cleanup дают повтор | `session_id` и idempotency key, серверная дедупликация |
| Durable result queue | Сеть блокирует обработку или требует повторного AI | `pcrt-result-queue` сохраняет готовый payload до удаления видео |
| Отдельный uploader | Сеть блокирует обработку или требует повторного AI | Отдельная служба читает SQLite result queue; только она имеет API credentials |
| Явный API client | HTTP-формат смешан с очередью, retry и AI | `pcrt-api-client` делает одну попытку, uploader владеет retry и SQLite state |
| Удаление видео | Архив обработанных MKV заполняет диск без пользы для доставки | После SQLite commit processor удаляет видеокаталог; recovery сверяет `session_id` с очередью и удаляет остаток после restart |
| Durable SQLite | Нет гарантии на offline/restart | WAL, `busy_timeout`, миграции, lease, backoff+jitter, DLQ и лимиты |
| Shutdown | Нет безопасного SIGTERM | Cancellation, bounded close ffmpeg, возврат не начатого claim |
| Secrets/TLS | Секреты в checkout, API HTTP | Отозвать текущий ключ, systemd credentials, HTTPS/CA validation |
| Least privilege | Демоны root, socket `0666` | Пользователь `pcrt`, group socket `0660`, systemd sandboxing |

## P1: в первых работающих сервисах

| Улучшение | Ожидаемый результат |
| --- | --- |
| Строгая schema config | Ошибка конфигурации обнаруживается до запуска, не скрывается default-значением |
| Health/readiness/metrics | Виден возраст очереди, stale door, retries, DLQ, заполнение диска, время inference/upload |
| Контракты fixtures | Совместимость RS-232, JSON и HTTP проверяется без физического автобуса |
| Fault injection | Тестируются reboot между fsync/rename, недоступная сеть, SQLite lock и SIGTERM |
| Версионируемые manifest/API | Возможны обновления без неявного слома накопленных сессий |
| Ограничение ресурсов | Размер очереди/видео и retention не позволяют заполнить диск |

## P2: после стабилизации транспорта и хранения

| Улучшение | Подход |
| --- | --- |
| Recorder | Оставить ffmpeg кодером, менять только lifecycle и надёжность сессии |
| AI pipeline | Сначала перенести result queue; OpenVINO/tracker - за trait `InferenceBackend` |
| Эталонный датасет | Набор MKV с ожидаемыми `in/out`; новый pipeline сравнивается с baseline |
| Shadow-run | Новый gateway/processor работает параллельно, но не влияет на production output |
| Поставка | Воспроизводимые release-бинарники, конфигурация отдельно от кода, rollback runbook |

## Принятые ограничения

- Не делать ровно-однократную HTTP-доставку: на ненадёжной сети это недостижимо без
  распределённой транзакции. Использовать at-least-once + API deduplication.
- Не переносить AI ради единообразия языка. Точность подсчёта важнее Rust-only стека.
- Не заменять ffmpeg на bindings без измеримой причины; отдельный процесс ffmpeg
  остаётся более прозрачной и проверенной границей.
- Не объединять все демоны в один процесс: сбой камеры или модели не должен лишать
  систему door telemetry, мониторинга и доставки накопленных результатов.
