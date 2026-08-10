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

Paused processor не claim-ит ready session. `pcrt-processor` binary владеет
aggregate ZeroMQ subscriber, local receive TTL и exclusive lock для одного
`SESSIONS_DIR`. Door gate вычисляется непосредственно перед `process_one`.
После успешного claim backend получает только `ClaimedSession` и
`ShutdownToken`: изменение `all_closed` или переход door state в `stale` не
отменяет уже начатую processing session. Только service shutdown может
отменить inference и вернуть claim в `ready`.

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

## Binary

```text
bins/pcrt-processor/src/
|-- config.rs          # typed env/CLI configuration
|-- runtime.rs         # lock, recovery, subscriber loop and shutdown
`-- processor/
    |-- gate.rs        # all-closed admission policy for a new claim only
    `-- lock.rs        # exclusive sessions-root ownership
```

The binary does not start a placeholder AI backend: doing so could claim and
incorrectly fail production sessions. Native decode/OpenVINO is added only
after fixture baseline and shadow comparison with the Python implementation;
then it is injected into the existing generic runtime as `InferenceBackend`.

## Configuration

`pcrt-processor` uses the same precedence as the other Rust binaries:
defaults, `config.env`, `processor.env`, environment and CLI.

| Key | Default | Meaning |
| --- | --- | --- |
| `SESSIONS_DIR` | `sessions` | Root owned by recorder/processor lifecycle. |
| `RESULT_QUEUE_DB` | `$SESSIONS_DIR/outbox/results.sqlite` | Durable uploader queue. |
| `ZMQ_IPC_ENDPOINT` | `ipc:///run/doors.sock` | Aggregate `doors.state` subscriber endpoint. |
| `DOOR_STATE_TTL_SEC` | `2` | Maximum local age of a valid aggregate state before new claims pause. |
| `IDLE_SLEEP` | `0.1` | Sleep while paused or no ready sessions exist. |
