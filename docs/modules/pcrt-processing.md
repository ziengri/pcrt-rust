# pcrt-processing

## Назначение

`pcrt-processing` координирует verified session storage, AI backend и durable
result queue. Crate не зависит от ZeroMQ, OpenCV, OpenVINO, HTTP или API
credentials. Эти зависимости подключаются через adapters и binary composition.

## Контракты

AI реализует `InferenceBackend` и получает только `ClaimedSession`, manifest
которой уже проверен `pcrt-storage`. Результат AI является domain type
`PassengerCounts`; transport payload создаёт отдельный `ResultEncoder`.

Первая версия поддерживает ровно одно video на session. Пустой или multi-video
manifest является terminal processing error и сохраняется в `failed`.

Перед `process_one` caller обязан вычислить door gate:

```text
latest aggregate state received
AND local receive TTL is fresh
AND stale == false
AND all_closed == true
```

Paused processor не claim-ит ready session. Конкретный ZeroMQ subscriber будет
добавлен в `pcrt-processor` binary отдельным этапом.

## Durable protocol

После успешного AI processor выполняет только следующий порядок:

```text
queue.insert(prepared)
-> storage.delete_claimed
-> queue.publish
```

После успешного `insert` AI для `session_id` больше не запускается. Ошибка delete
или publish оставляет `prepared` row для startup recovery. При ошибке самого
insert claim не освобождается: commit outcome может быть неизвестен, поэтому
recovery сначала проверяет queue.

Terminal AI/result encoding error переводит claim в `failed`. Cancellation до
durable result возвращает claim в `ready`.

## Startup recovery

`Processor::recover` должен выполняться до processing loop и при exclusive
processor ownership:

1. Получить все `prepared_session_ids`.
2. Idempotently удалить matching `claimed` directory.
3. Опубликовать row в `pending`.
4. После reconciliation вызвать `recover_processing`, чтобы вернуть остальные
   abandoned claims в `ready`.

Наличие queue row проверяется повторно после каждого claim. Если результат уже
существует, video удаляется и AI не запускается.

## Следующий этап

Следующий slice добавит reusable aggregate door subscriber, exclusive process
lock и `pcrt-processor` binary. Native decode/OpenVINO backend подключается только
после fixture baseline и shadow comparison с Python implementation.
