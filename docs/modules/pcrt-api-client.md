# pcrt-api-client

## Назначение

`pcrt-api-client` выполняет ровно одну попытку записи готового результата
пассажиропотока в внешний API. Он не читает SQLite, не управляет очередью и не
делает скрытых повторов. Решение о повторной отправке принадлежит
`pcrt-uploader`.

Текущий совместимый контракт извлечён из Python
`app/processing/result_sink.py` и опубликованного OpenAPI. Его версия хранится в
[`contracts/api/timeline-v1.md`](../../contracts/api/timeline-v1.md).

## Границы

Crate отвечает за:

- чтение `API_BASE_URL` и `API_X_AUTH` из environment;
- timeout, TLS-настройки стандартного HTTP-клиента и запрет redirects;
- валидацию JSON из result queue;
- кодирование JSON в `application/x-www-form-urlencoded`;
- `POST /api/v1/timeline` с `X-AUTH` и `Idempotency-Key`;
- классификацию одного результата HTTP-вызова.

Crate не отвечает за:

- SQLite, очередь, retry, backoff, jitter и dead letter;
- формирование AI-результата;
- файловые сессии и видео;
- автоматическое создание автобуса через `/api/v1/buses`.

Python-реализация выполняет изменяющий `POST /api/v1/buses`, когда шина не
найдена. Новая реализация намеренно этого не делает: provisioning шины не должен
быть скрытым побочным эффектом доставки результата.

## Конфигурация

| Переменная | Назначение |
| --- | --- |
| `API_BASE_URL` | Корневой URL API, например `http://api.gortransportnch.ru:8000`. |
| `API_X_AUTH` | Credential для заголовка `X-AUTH`; значение не выводится клиентом. |

`ApiClientConfig::from_environment()` использует timeout 10 секунд.
`ApiClientConfig::new()` позволяет service задать иной timeout явно.

Поддерживаются URL только с `http` и `https`, без query или fragment. Production
должен использовать HTTPS, когда endpoint предоставляет TLS.

## Запрос

```text
POST /api/v1/timeline
Accept: application/json
Content-Type: application/x-www-form-urlencoded
X-AUTH: <credential>
Idempotency-Key: pcrt-result:<session_id>
```

Очередь хранит JSON следующей формы:

```json
{
  "bus": "tst000",
  "cam": 1,
  "date": "29.07.2026T12:34",
  "in": 3,
  "out": 1
}
```

Клиент отклоняет неизвестные поля, пустые `bus` и `date`, невалидный JSON и
пустой/некорректный HTTP `Idempotency-Key`. Валидный payload отправляется в
точности как form body:

```text
bus=tst000&cam=1&date=29.07.2026T12%3A34&in=3&out=1
```

## Результат одной попытки

`TimelineApiClient::send_timeline()` возвращает `DeliveryOutcome`:

| Outcome | Условие | Действие uploader |
| --- | --- | --- |
| `Delivered` | Любой `2xx` | `result_queue.delete(session_id)` |
| `Retryable` | Network/timeout, `408`, `425`, `429`, `5xx` | Рассчитать backoff и вызвать `reschedule`. |
| `Permanent` | Невалидный локальный payload, иной HTTP-код | Вызвать `dead_letter`. |

Тело HTTP-ответа не добавляется в диагностическое сообщение, чтобы не сохранять
непроверенные или потенциально чувствительные данные в SQLite и логах.

## Идемпотентность

Клиент всегда передаёт `Idempotency-Key`, но текущая OpenAPI-спецификация API не
документирует серверную дедупликацию по этому заголовку. Следовательно, повтор
после сбоя между успешным API-ответом и локальным `delete` может создать дубликат
timeline-записи, пока сервер не подтвердит поддержку ключа. Это production blocker
для полной at-least-once гарантии.

## Проверки

Unit-тесты проверяют:

- form-кодирование payload;
- запрет неизвестных полей;
- классификацию HTTP-кодов;
- отсутствие вывода секрета в ошибки конфигурации;
- фактический локальный HTTP-запрос: method, path, form body и заголовки.

Live API проверялся 29.07.2026 только безопасными операциями. `GET /health`
вернул `200`. Документированный read-only `POST /api/v1/passengers` с
`bus=tst000` вернул `404`; запись в `/api/v1/timeline` не выполнялась. Перед
включением uploader требуется сверить текущий deployment с опубликованным
OpenAPI и подтвердить серверную идемпотентность.
