# pcrt-uploader

## Назначение

`pcrt-uploader` - синхронный delivery loop для одной SQLite result queue. Он
читает готовые `pending`-результаты, передаёт их через `pcrt-api-client` и
сохраняет решение об успешной доставке, retry или dead letter.

```text
pcrt-result-queue -> pcrt-uploader -> pcrt-api-client -> Passenger Flow API
```

Uploader рассчитан на одну service instance. Несколько uploader, работающих с
одной SQLite-базой, не поддерживаются: queue не использует lease.

## Зависимости и границы

| Crate | Использование |
| --- | --- |
| `pcrt-result-queue` | `next_due`, `delete`, `reschedule`, `dead_letter`. |
| `pcrt-api-client` | Одна HTTP-попытка через `TimelineDelivery`. |
| `pcrt-service` | Проверка `ShutdownToken` в длительном loop. |

Uploader владеет retry policy. Он не владеет HTTP, credentials, SQLite-миргациями,
AI, видео, формированием payload или обработкой `prepared` сессий.

## Одна итерация

`process_next(now)` предназначен для детерминированного вызова из тестов и
сервисного loop:

```text
next_due(now)
  |
  +-> None: Idle
  |
  +-> pending entry
        |
        +-> Delivered: delete(session_id)
        |
        +-> Retryable: reschedule(session_id, retry_at, error)
        |
        +-> Permanent: dead_letter(session_id, error)
```

`prepared`-записи не возвращаются `next_due`, поэтому uploader не может
отправить результат до завершения удаления видео processor-ом.

## Retry Policy

Значения по умолчанию:

| Настройка | Значение |
| --- | --- |
| `poll_interval` | 1 секунда |
| `initial_backoff` | 5 секунд |
| `max_backoff` | 15 минут |
| jitter | full jitter |

Верхняя граница задержки для `n`-й неуспешной попытки:

```text
min(max_backoff, initial_backoff * 2^(n - 1))
```

Full jitter выбирает итоговую задержку от нуля до этой границы. Он предотвращает
одновременный retry нескольких устройств после восстановления API.

`attempts` в queue - диагностическое число неудач. Оно не ограничивает повторную
доставку: network/timeout, `429` и `5xx` продолжают повторяться бесконечно с
задержкой не более `max_backoff`. Это необходимо для бортового ПК, который может
долго находиться без интернета.

## Классификация результата

| `DeliveryOutcome` | Действие |
| --- | --- |
| `Delivered` | Удалить строку из queue. |
| `Retryable` | Рассчитать backoff с jitter и вызвать `reschedule` без лимита попыток. |
| `Permanent` | Немедленно вызвать `dead_letter`. |

Ошибки операций queue не меняют состояние сообщения дополнительно. Uploader
возвращает ошибку supervisor-у; строка остаётся в SQLite и при следующем старте
будет обработана снова.

## Shutdown

`run_until_shutdown` проверяет `ShutdownToken` перед каждой новой итерацией.
После `SIGTERM` signal adapter бинарника запрашивает shutdown, uploader больше
не берёт новую строку и завершает работу после текущей синхронной HTTP-попытки.

Если процесс остановлен до `delete` после API-ответа, запись остаётся в queue и
будет отправлена повторно. Это ожидаемая at-least-once семантика.

## Idempotency

Uploader считает `Idempotency-Key` работающим контрактом API. Ключ стабильно
хранится вместе с payload в result queue и передаётся в каждую попытку. API
должен дедуплицировать повторные записи по этому ключу.

## Тесты

Unit-тесты покрывают:

- пустую queue без HTTP-вызова;
- `Delivered` и удаление строки;
- retry с детерминированным jitter;
- повтор временной ошибки после многих попыток;
- permanent error и immediate dead letter;
- скрытие `prepared` от uploader;
- ограничение exponential backoff;
- валидацию retry policy.
