# Timeline API v1

Источник контракта: `http://api.gortransportnch.ru:8000/openapi.json`, проверен
29.07.2026. Этот файл фиксирует только контракт передачи результата
пассажиропотока, используемый `pcrt-api-client`.

## Запись результата

```text
POST /api/v1/timeline
Accept: application/json
Content-Type: application/x-www-form-urlencoded
X-AUTH: <API_X_AUTH>
Idempotency-Key: pcrt-result:<session_id>
```

Тело form-urlencoded содержит ровно следующие обязательные поля:

| Поле | Тип | Пример |
| --- | --- | --- |
| `bus` | string | `tst000` |
| `cam` | integer | `1` |
| `date` | string | `29.07.2026T12:34` |
| `in` | integer | `3` |
| `out` | integer | `1` |

В SQLite result queue исходный payload сохраняется как JSON, соответствующий
`timeline-v1.schema.json`. `pcrt-api-client` строго проверяет этот JSON и
кодирует те же значения как form body; uploader не изменяет результат.

## Ответы

OpenAPI документирует:

- `201 Created`: `{"status":"ok","data":{"bus","cam","date","in","out"}}`;
- `422 Unprocessable Entity`: FastAPI validation error.

Клиент считает любой `2xx` успешной доставкой. Статусы `408`, `425`, `429` и
`5xx`, а также ошибки сети/timeout являются временными. Остальные статусы и
некорректный локальный payload являются постоянными ошибками.

## Идемпотентность

`Idempotency-Key` передаётся клиентом для каждого результата и строится из
стабильного `session_id`. Однако текущая OpenAPI-спецификация не документирует
серверную дедупликацию по этому заголовку. Пока сервер не подтвердит или не
реализует её, повтор после сбоя между API-ответом и локальным `delete` может
создать дубликат timeline-записи. Это production blocker для at-least-once
доставки.

## Безопасная проверка

Тестовая шина: только `tst000`. Запись в `/api/v1/timeline` является изменяющей
операцией и не используется для smoke-проверок. Разрешённая read-only проверка:

```text
POST /api/v1/passengers
bus=tst000&cam=1&dateFrom=01.01.2000T00:00&dateTo=31.12.2099T23:59
```

Она возвращает существующую статистику и не создаёт timeline-запись.
