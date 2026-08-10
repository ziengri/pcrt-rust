# Архитектура PCRT Rust

## Цель

Построить надёжную систему подсчёта пассажиропотока для бортового компьютера.
Система должна переживать отключение сети и питания, не терять готовые сессии и
не создавать несколько логических результатов для одной поездки через дверь.

Python-реализация остаётся источником совместимых контрактов на период миграции.
Rust-компонент заменяется в production только после shadow-run и сверки результатов.

## Границы процессов

Будущая система сохраняет отдельные systemd-службы:

| Бинарник | Ответственность | Внешние зависимости |
| --- | --- | --- |
| `pcrt-door-gateway` | RS-232, состояние дверей, ZeroMQ publish | serial, libzmq |
| `pcrt-recorder` | RTSP, управление ffmpeg, фиксация сессии | камера, ffmpeg, ZeroMQ |
| `pcrt-processor` | claim сессии, inference, подсчёт, insert результата и удаление видео | модель, storage |
| `pcrt-uploader` | независимая доставка результатов из result queue | SQLite, HTTPS API |
| `pcrt-monitor` | health, systemd/journal, события | systemd, storage, HTTPS API |
| `pcrtctl` | проверка конфигурации и диагностика | локальные state/runtime dirs |

`pcrt-uploader` является обязательной отдельной службой. Это устраняет зависимость
доставки от поступления следующей сессии, изолирует сетевые сбои от AI и делает его
единственным владельцем API URL и credentials для результатов пассажиропотока.

## Структура workspace

```text
rust/
  crates/
    pcrt-model/       # сущности системы и бизнес-инварианты
    pcrt-config/      # источники, валидация и redacted-конфигурация
    pcrt-service/     # lifecycle служб: SIGTERM, shutdown, health
    pcrt-storage/     # manifest, атомарный session-dir и recovery видео
    pcrt-result-queue/# SQLite-очередь готовых результатов для uploader
    pcrt-api-client/  # HTTP-клиент API пассажиропотока, auth, DTO и ответы
    pcrt-door/        # RS-232 parser/FSM и ZeroMQ publisher/subscriber
    pcrt-recording/   # камера, ffmpeg и recorder FSM
    pcrt-processing/  # очередь, линии подсчёта, InferenceBackend
    pcrt-monitoring/  # probes, journal, transition rules
  bins/
    pcrt-door-gateway/
    pcrt-recorder/
    pcrt-processor/    # private processor runtime, gate, lock and native AI adapters
    pcrt-uploader/
    pcrt-monitor/
    pcrtctl/
  contracts/
    door/             # binary frames, ZeroMQ JSON fixtures
    session/          # JSON Schema manifest и fixtures
    api/              # OpenAPI/JSON Schema результатов и событий
  integration-tests/
    recovery/
    transport/
    system/
```

Сейчас созданы `pcrt-model`, `pcrt-config`, `pcrt-service`, `pcrt-storage`,
`pcrt-result-queue`, `pcrt-api-client` и `pcrt-uploader`. Остальные crate
добавляются вместе с первым работающим потребителем, а не заранее пустыми.

## Правила зависимостей

```text
pcrt-model        <- pcrt-storage       <- pcrt-recording / pcrt-processing
                  <- pcrt-result-queue  <- pcrt-processing / pcrt-uploader
                  <- pcrt-uploader      <- pcrt-api-client
pcrt-api-client  <- pcrt-uploader
pcrt-config     <- все библиотеки и бинарники
pcrt-service    <- сервисные библиотеки и бинарники
pcrt-door       <- pcrt-recorder
```

- `pcrt-model` не знает о файловой системе, SQLite, HTTP, ZeroMQ, ffmpeg или OpenVINO.
- `pcrt-storage` владеет только файловыми сессиями и не обращается к SQLite.
- `pcrt-result-queue` владеет SQLite, схемой очереди, миграциями и состоянием строк.
- Только `pcrt-result-queue` создаёт, выдаёт, удаляет и переносит в DLQ сообщения
  результатов.
- `pcrt-api-client` определяет HTTP-запросы, TLS, auth и DTO API, но не читает SQLite
  и не принимает решение о повторной отправке.
- `pcrt-uploader` владеет backoff и jitter, а для одной отправки вызывает
  `pcrt-api-client`; временные ошибки повторяются без лимита попыток.
- `pcrt-processing` не зависит от `pcrt-api-client` и не получает API credentials.
- `pcrt-uploader` - единственный отправитель результатов пассажиропотока в API.
- Бинарники связывают зависимости и не содержат бизнес-правил.
- AI реализуется через `InferenceBackend`; обработчик очереди не зависит от OpenVINO
  или конкретного tracker.

## Сессия и восстановление

Единица работы - каталог сессии, а не пара независимо перемещаемых файлов:

```text
state/sessions/
  capturing/<session_id>.tmp/  # видео пишется сюда
  ready/<session_id>/          # atomically renamed после fsync
  claimed/<session_id>/        # временная lease обработчика
  failed/<session_id>/         # terminal failure с причиной
```

Внутри находятся `manifest.json` и видео. Manifest содержит `schema_version`,
стабильный `session_id`, параметры видео, контрольные суммы/размеры и историю
переходов. Путь к видео всегда относителен
каталогу сессии; абсолютные пути и `..` запрещены.

Фиксация готовой сессии: записать данные во временный каталог, выполнить `fsync`
файлов и каталога, затем сделать атомарный rename всего каталога в `ready`.
При старте storage выполняет recovery вместо безусловного удаления `active` или
`processing`: валидные сессии возвращаются в допустимое состояние, повреждённые
помещаются в `failed` с причиной.

Успешно обработанная сессия не переносится в `processed` или `saved`. Processor
сначала добавляет результат в `pcrt-result-queue`, затем удаляет её каталог с
видео. SQLite не хранит lifecycle видео или результат как отдельную сущность: это
только durable очередь сообщений для uploader. В строке очереди хранится
уникальный `session_id`; если процесс остановлен между `INSERT` и удалением
каталога, recovery получает `session_id` из очереди и безопасно удаляет оставшееся
видео. Если `INSERT` не состоялся, сессия возвращается в `ready` или переносится
в `failed` согласно причине ошибки.

## Состояния и доставка

Файловая сессия и SQLite-очередь доставки имеют независимые lifecycle. Это
исключает повторное выполнение AI только из-за сетевой ошибки.

Lifecycle сессии определён в `pcrt-model`:

```text
capturing -> ready -> claimed
                 |        |
                 +--------+-> failed
```

`claimed -> ready` допускается при истёкшей lease или штатном shutdown до начала
необратимой обработки. После успешного AI processor создаёт в `pcrt-result-queue`
одну `prepared`-запись с `session_id`, idempotency key и готовым `payload_json`.
После commit он удаляет каталог с видео, переводит строку в `pending` и никогда не
выполняет HTTP-запросы. Пока строка `prepared`, uploader её не видит. Это исключает
ситуацию, когда uploader удалил результат между SQLite commit и удалением видео.
При restart recovery находит `prepared` по `session_id`, удаляет оставшийся каталог
и переводит строку в `pending`. Наличие строки с данным `session_id` означает, что
AI повторно запускать нельзя.

Lifecycle записи result queue при одном uploader:

```text
prepared -> pending -> deleted  # успешный HTTP-ответ
               |
               +-> pending      # retryable error или restart uploader
               |
               +-> dead_letter  # terminal error
```

Uploader читает первую `pending`-запись с наступившим `next_attempt_at`, отправляет
сохранённый `payload_json` и после успешного ответа удаляет строку. При временной
ошибке он назначает новую `next_attempt_at`; при terminal error переносит строку в
`dead_letter`. Единственный uploader не требует lease. Каждая строка имеет
уникальные `session_id` и idempotency key; HTTP-сервер обязан дедуплицировать
повторный запрос по этому ключу. Удаление меняет только строку SQLite:
видеокаталог уже удалён processor.

`pcrt-api-client` выполняет только одну попытку передачи: формирует запрос,
добавляет `Idempotency-Key` и credentials, применяет TLS/timeout и возвращает
классифицированный ответ. Он не обращается к SQLite и не делает скрытых повторов;
иначе uploader потеряет контроль над состоянием очереди и retry policy.

## Очередь результатов

`pcrt-result-queue` - единственный crate с доступом к SQLite-базе результатов.
Он не знает о видео, AI, HTTP, credentials и политике расчёта backoff. Его API
содержит только устойчивые операции очереди:

```text
insert(session_id, idempotency_key, payload_json)
publish(session_id)
contains_session(session_id)
next_due(now)
delete(session_id)
reschedule(session_id, retry_at, error)
dead_letter(session_id, error)
```

`insert` идемпотентен по `session_id`: повторная вставка того же готового результата
не создаёт вторую строку. `session_id` и `idempotency_key` уникальны. `payload_json`
является непрозрачным готовым сообщением и не пересобирается uploader-ом. Очередь
сохраняет `prepared`, `pending` и `dead_letter`, число попыток, время следующей
попытки и диагностическую ошибку. Uploader выбирает backoff и jitter; queue
атомарно сохраняет это решение. Число попыток является диагностическим и не
переводит временную ошибку в `dead_letter`.

## Модули предметной области

`pcrt-model` разделяется по устойчивым понятиям системы, а не по техническим
слоям:

```text
pcrt-model/src/
  door.rs       # DoorId, DoorState, DoorTelemetry
  session.rs    # SessionId, SessionState и допустимые переходы
  counting.rs   # будущие PassengerCount, Direction, ProcessingResult
  camera.rs     # будущие CameraId и связь камеры с дверью
```

HTTP DTO не входят в `pcrt-model`: внешний API может иметь другие имена полей,
версии и форматы. Преобразование `ProcessingResult` в JSON-запрос принадлежит
`pcrt-api-client`, что не позволяет изменениям API менять бизнес-модель.

Доставка имеет семантику at-least-once: повторный запрос допустим, потеря
логического результата недопустима.

## Конфигурация и эксплуатация

Порядок переопределения единый для всех бинарников:

```text
defaults < config file < device file < environment < CLI
```

Секрет является ссылкой на credential, а не строковым значением конфигурации.
Production-службы читают systemd credentials или root-owned файл вне checkout.
`pcrtctl check-config` валидирует итоговую конфигурацию и выводит её только в
redacted-виде.

Каждый сервис обрабатывает `SIGTERM`: останавливает приём новой работы, завершает
или безопасно откатывает текущую операцию в пределах лимита systemd и освобождает
ресурсы. В systemd используются отдельный пользователь `pcrt`, `StateDirectory`,
`RuntimeDirectory`, минимальные права на socket и `Restart=on-failure`.

## Контракты до миграции

Первой мигрируется дверь. Rust gateway обязан сохранить:

- binary RS-232 prefix `!DOORS:` и фиксированные размеры 28/35 байт;
- IPC endpoint `ipc:///run/doors.sock` на этапе совместной работы;
- ZeroMQ topics `doors.state` и `door.N.state`;
- существующую JSON-схему `seq`, `ts`, `doors`, `any_open`, `all_closed`, `stale`.

Fixtures для этих контрактов добавляются в `contracts/door` до написания gateway.
