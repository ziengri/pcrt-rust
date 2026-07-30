# pcrt-result-queue

## Назначение

`pcrt-result-queue` - надёжная локальная SQLite-очередь готовых результатов
пассажиропотока. Она отделяет завершение AI-обработки от доступности сети и API.

Processor записывает в неё готовый JSON-результат до удаления видео. Uploader
читает результат позднее и отправляет его в API. Поэтому недоступный API не
запускает AI повторно и не требует хранить видео до восстановления сети.

## Границы

Crate отвечает за:

- SQLite-файл, миграции и настройки долговечности;
- уникальность `session_id` и `idempotency_key`;
- хранение непрозрачного `payload_json`;
- выбор следующей доступной записи;
- сохранение попыток, времени повтора и последней ошибки;
- dead letter для результатов без автоматических повторов.

Crate не отвечает за:

- видео, manifest, каталоги сессий и удаление файлов;
- AI, подсчёт пассажиров и формирование JSON;
- HTTP, TLS, API credentials и классификацию HTTP-ответов;
- расчёт backoff, jitter и лимита попыток.

`pcrt-storage` владеет файлами видео. `pcrt-processing` создаёт payload.
`pcrt-uploader` выбирает retry policy и вызывает `pcrt-api-client`.

## Модель доставки

Очередь рассчитана на один uploader. Lease не используется.

```text
processor                         result queue                 uploader
---------                         ------------                 --------
AI result -> insert(prepared) -> SQLite
delete video -> publish --------> pending --------------------> next_due
                                                                  |
                                             reschedule <---------+ retryable error
                                             dead_letter <--------+ terminal error
                                             delete <-------------+ successful API response
```

После успешного HTTP-ответа uploader вызывает `delete`. Если он остановится между
ответом API и `delete`, та же строка будет отправлена повторно после рестарта.
Это ожидаемая семантика at-least-once. API должен дедуплицировать запросы по
`Idempotency-Key`.

## Барьер Prepared

Processor выполняет операции строго в таком порядке:

1. Выполнить AI и сформировать `payload_json`.
2. Вызвать `insert`; запись сохранится в состоянии `prepared`.
3. Удалить видеокаталог.
4. Вызвать `publish`; запись перейдёт в `pending` и станет видимой uploader.

`prepared` необходим для сбоя между записью SQLite и удалением видео. Uploader не
может отправить или удалить такую строку раньше времени. При старте recovery делает
следующее для каждого `prepared_session_ids()`:

1. Находит каталог видео по `session_id`.
2. Удаляет каталог, если он остался.
3. Вызывает `publish(session_id)`.

Если строка с `session_id` существует в queue, AI для этого видео нельзя запускать
повторно.

## Состояния

```text
prepared -> pending -> deleted
               |
               +-> pending      # временная ошибка, новая next_attempt_at
               |
               +-> dead_letter  # постоянная ошибка
```

- `prepared`: результат надёжно записан, но видео ещё не подтверждено удалённым.
- `pending`: результат доступен uploader.
- `dead_letter`: автоматическая доставка остановлена, запись остаётся для
  диагностики и ручного решения.
- `deleted`: состояние не хранится, строка удалена после успешного ответа API.

## Публичный API

| Метод | Вызывает | Назначение |
| --- | --- | --- |
| `open(path)` | processor, uploader, recovery | Открывает базу, создаёт каталог, применяет миграции. |
| `insert(session_id, idempotency_key, payload_json, now)` | processor | Сохраняет результат в `prepared`; повтор идентичного insert безопасен. |
| `publish(session_id)` | processor, recovery | Переводит `prepared` в `pending`; повторный вызов безопасен. |
| `contains_session(session_id)` | recovery | Проверяет, был ли результат уже сохранён. |
| `prepared_session_ids()` | recovery | Возвращает сессии, для которых нужно завершить удаление видео. |
| `next_due(now)` | uploader | Возвращает самую старую `pending` запись с наступившим `next_attempt_at`. |
| `reschedule(session_id, attempted_at, retry_at, error)` | uploader | Увеличивает attempts и назначает следующую попытку. |
| `dead_letter(session_id, attempted_at, error)` | uploader | Останавливает автоматическую доставку. |
| `delete(session_id)` | uploader | Удаляет подтверждённый API результат. |

## Идемпотентность

`session_id` и `idempotency_key` имеют уникальные ограничения.

Повторный `insert` с теми же `session_id`, ключом и payload возвращает
`InsertOutcome::Existing` и не создаёт дубликат. Повтор с другим payload или ключом
возвращает ошибку `ConflictingSessionResult`: это признак нарушения инварианта,
который нельзя молча игнорировать.

Ключ рекомендуется формировать стабильно:

```text
pcrt-result:<session_id>
```

Uploader передаёт ключ в HTTP-заголовке `Idempotency-Key`.

## SQLite и схема

При открытии включаются:

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = FULL;
PRAGMA foreign_keys = ON;
```

Ожидание SQLite lock ограничено `busy_timeout = 5s`.

Текущая схема версии 1:

```sql
CREATE TABLE result_queue (
    session_id TEXT PRIMARY KEY NOT NULL,
    idempotency_key TEXT NOT NULL UNIQUE,
    payload_json TEXT NOT NULL,
    state TEXT NOT NULL, -- prepared, pending, dead_letter
    created_at_ms INTEGER NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at_ms INTEGER NOT NULL,
    last_attempt_at_ms INTEGER,
    last_error TEXT,
    dead_letter_at_ms INTEGER
);
```

`payload_json` не разбирается и не пересобирается в queue. Это сохраняет исходный
API-контракт результата, даже если код processor или DTO будет обновлён до доставки
накопленных сообщений.

## Ошибки и ограничения

- `QueueError::ConflictingSessionResult`: один `session_id` попытались связать с
  разными результатами. Это ошибка processor или recovery.
- `QueueError::MissingPendingMessage`: uploader попытался изменить отсутствующую,
  `prepared` или уже dead-letter строку.
- `dead_letter` не возвращается в очередь автоматически. Ручный retry появится
  позднее через `pcrtctl`.
- Конкурентные uploader не поддерживаются. При появлении второго uploader API
  нужно заменить `next_due` на атомарный lease.
- Локальная очередь не является архивом. После успешного API-ответа строка
  удаляется; долгосрочная история принадлежит серверу.

## Проверки

Unit-тесты crate покрывают:

- hidden `prepared` до `publish`;
- идемпотентные `insert` и `publish`;
- повтор после заданного `next_attempt_at`;
- исключение dead letter из выдачи;
- удаление после успешной доставки;
- сохранность записей после закрытия и повторного открытия SQLite-базы.

Проверка workspace:

```sh
cargo fmt --all --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```
