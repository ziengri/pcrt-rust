# pcrt-door

## Статус и решение

Этот документ фиксирует границы и контракты первого Rust-модуля для двери до
реализации. Он заменит Python `door_gateway` только после shadow-run на реальном
контроллере. Python остаётся production-владельцем `/run/doors.sock` до явного
переключения.

Door subsystem состоит из private gateway logic, shared `ZeroMQ` door-bus crate и
тонких consumer policies:

```text
SerialPortReader -> raw byte chunks -> private protocol decoder -> private DoorStateMachine
                                                                         |
                                                                         v
                                              pcrt-door-zmq::DoorsState -> publisher
```

Private logic `pcrt-door-gateway` не доступна другим production binaries:
controller packet format, decoder, FSM, reconnect/liveness policy, source I/O and
monotonic clock принадлежат gateway. `pcrt-door-zmq` владеет shared `DoorsState`,
PUB/SUB sockets, JSON/topics and safe IPC endpoint lifecycle. Recorder и processor
получают только aggregate `DoorsState` через shared subscriber и применяют свою
private policy. Это позволяет детерминированно тестировать gateway parser/FSM без
контроллера двери и не дублировать transport contract в consumers.

## Неизменяемые compatibility contracts

До production switch Rust сохраняет следующие текущие contracts:

| Область | Contract |
| --- | --- |
| Serial prefix | ASCII `!DOORS:` |
| Door count | ровно 3 или 4, IDs `1..=count` |
| Packet length | 25 bytes для 3 дверей, 31 bytes для 4 |
| Door record | `<id>=<state>,<voltage>;`, ровно 6 bytes |
| State | raw byte `0` = closed, `1` = open |
| Voltage | один raw byte, `u8`, unit неизвестна |
| Aggregate topic | `doors.state` |
| Per-door topic | `door.N.state` |
| ZMQ wire form | одна UTF-8 frame: `<topic><space><compact JSON>` |
| Default endpoint | `ipc:///run/doors.sock` |
| Payload fields | `seq`, `ts`, `doors`, `any_open`, `all_closed`, `stale` |

Формат подтверждён captured live RS-232 packet 2026-07-30. Rust расширяет wire
`u8` до output `u16`, чтобы сохранить числовой JSON schema consumers.

## Public library API

`pcrt-door` зависит только от `pcrt-model`. Первый API:

```rust
pub struct DoorProtocol { /* configured door_count: 3 | 4 */ }
pub struct DoorPacket { /* BTreeMap<DoorId, DoorTelemetry> */ }
pub struct StreamDecoder { /* bounded byte buffer */ }
pub struct DoorSnapshot { /* sequence, values, stale */ }
pub struct DoorStateMachine { /* snapshot state + monotonic last_packet */ }

impl DoorProtocol {
    pub fn parse_packet(&self, bytes: &[u8]) -> Result<DoorPacket, DoorProtocolError>;
}
impl StreamDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<DecodeEvent>;
}
impl DoorStateMachine {
    pub fn initial(&self, now: MonotonicInstant) -> DoorSnapshot;
    pub fn accept(&mut self, packet: DoorPacket, now: MonotonicInstant) -> DoorSnapshot;
    pub fn mark_stale_if_due(&mut self, now: MonotonicInstant) -> Option<DoorSnapshot>;
}
```

Точные имена могут минимально меняться до реализации, но ownership остаётся:

- `DoorProtocol` валидирует один полный fixed-size packet;
- `StreamDecoder` никогда не интерпретирует состояние и не владеет clock;
- `DoorStateMachine` принимает только валидный полный `DoorPacket`;
- publisher получает готовый `DoorSnapshot` и не вычисляет business state.

`DoorSnapshot` использует [`DoorId`](../../crates/pcrt-model/src/door.rs) и
`DoorTelemetry`, `sequence: u64`, `stale: bool`. `all_closed` и `any_open`
вычисляются из полного snapshot, а не из delta. Wall-clock timestamp не входит в
FSM: publisher добавляет его при кодировании wire message.

## Byte source adapter и test transport

Serial port не является входом domain logic. Gateway получает последовательность
ограниченных raw chunks через технический adapter:

```rust
trait ByteSource {
    fn next(&mut self, deadline: MonotonicInstant) -> Result<ByteSourceEvent, ByteSourceError>;
    fn name(&self) -> &str;
}

enum ByteSourceEvent {
    Bytes(Vec<u8>),
    Timeout,
    Disconnected,
}
```

Это private adapter API `pcrt-door-gateway`, а не API `pcrt-door`: `pcrt-door`
получает только `&[u8]` в `StreamDecoder::push`. `ByteSource` не знает packet
prefix, door count, parser, FSM или ZeroMQ. Он не склеивает reads до packet и не
отбрасывает garbage. Размер `Bytes` ограничен read buffer, например 4 KiB.

Gateway main loop одинаково обрабатывает каждый source event:

1. `Bytes` передаётся без преобразования в `StreamDecoder`.
2. Decode events отдают valid packet в FSM; rejected/truncated events идут в metrics.
3. `Timeout` даёт loop проверить stale и heartbeat deadlines.
4. `Disconnected` закрывает source и запускает source-specific reconnect policy.

Первый production adapter - `SerialByteSource`. Для локальных и CI integration
tests добавляется `UnixStreamByteSource`, читающий bytes из Unix-domain stream.
Он доступен только с Cargo feature `test-transport`; production systemd unit не
передаёт этот флаг и не принимает test-source CLI options. Оба adapter-а обязаны
сохранять порядок и границы реальных reads: parser не может полагаться на то, что
один `Bytes` равен одному packet.

### Test protocol publisher

`pcrt-door-protocol-publisher` - dev/test binary, а не production service. Он
слушает отдельный Unix socket и отдаёт подключившемуся `UnixStreamByteSource`
заданный сценарий raw chunks. Этот socket полностью отличен от ZeroMQ output
endpoint gateway.

Сценарий содержит deterministic events:

```json
[
  {"after_ms": 0, "bytes_hex": "21444f4f52533a..."},
  {"after_ms": 10, "bytes_hex": "..."},
  {"after_ms": 20, "disconnect": true}
]
```

Он может отправлять любые bytes: partial prefix, packet по одному byte, garbage,
malformed candidate, несколько packet одним write и disconnect. `after_ms`
измеряется от предыдущего event; сценарий не использует
wall clock gateway и не меняет его FSM clock.

Publisher **не вызывает** `DoorProtocol::parse_packet` или `StreamDecoder` и не
проверяет, что bytes корректны. Основные сценарии состоят из статических fixture
bytes, записанных независимо от Rust parser. Поэтому parser, framing и resync
остаются частью system under test. Дополнительный convenience mode может кодировать
канонический valid frame из door values для demo/manual smoke test, но он не
используется для parser acceptance/rejection tests и не заменяет binary fixtures.

Интеграционный запуск без RS-232:

```text
pcrt-door-protocol-publisher --listen /tmp/door-source.sock --scenario valid-open.json
pcrt-door-gateway --test-byte-source-unix /tmp/door-source.sock \
  --ipc-endpoint ipc:///tmp/doors-test.sock
test subscriber --connect ipc:///tmp/doors-test.sock
```

Harness создаёт уникальные temporary socket paths, ожидает readiness обоих
процессов, собирает `doors.state` и `door.N.state`, затем сверяет их с snapshot
fixtures. Он также управляет monotonic test clock gateway через injected clock или
short deterministic durations; он не использует `sleep` как доказательство stale
границы. После теста harness проверяет exit status и удаление только своих socket
paths.

## Serial packet parser

Размер packet определяется конфигурацией до чтения:

```text
7-byte prefix + door_count * 6-byte records
```

Каждая запись разбирается строго по offsets, а не `split(';')`:

```text
offset + 0: ASCII '1'..'4'
offset + 1: '='
offset + 2: state byte 0 or 1
offset + 3: ','
offset + 4: voltage byte
offset + 5: ';'
```

ID могут идти в packet в любом порядке, но каждое ожидаемое ID
должно встретиться ровно один раз. Duplicate, missing/out-of-range ID, неверный
separator/state/prefix/length завершаются `DoorProtocolError`; partial packet не
считается invalid packet.

`StreamDecoder` хранит bounded buffer. Он:

1. Ищет prefix `!DOORS:`.
2. При отсутствии prefix сохраняет максимум последние шесть bytes, достаточные для
   prefix на границе read.
3. Удаляет garbage перед prefix.
4. Ждёт полный configured packet size.
5. Если следующий prefix встречен до expected end, emits truncation event и
   resync-ится на этот prefix.
6. Передаёт полный candidate в `DoorProtocol`.
7. После semantic error emits rejected event и пробует resync внутри consumed
   candidate, а затем в remaining buffer, чтобы valid packet после одной потери
   byte не терялся.

Шаг 7 намеренно улучшает Python, который может потерять valid packet после invalid
fixed-size candidate. Для обычного valid stream shadow-run требует точного
совпадения с Python. Для corruption/recovery stream comparator отдельно ожидает
фиксированные Rust fixtures и помечает дополнительное корректно восстановленное
состояние как approved divergence, а не как скрытый compatibility mismatch.

Buffer имеет фиксированный максимальный размер: `2 * max_packet_size + prefix_len`.
При overflow сбрасывается всё до возможного suffix-prefix и пишется rate-limited
diagnostic event. Ни один path не аллоцирует по непроверенной длине serial input.

Нет CRC в текущем wire contract, поэтому structurally valid corruption отличить
невозможно. Gateway считает такой packet valid, но инкрементирует protocol metrics;
CRC нельзя добавить до версии device protocol.

## FSM и время

Binary передаёт library монотонные durations, не `time.time()`.

Initial snapshot:

- все configured doors: closed, voltage 0;
- `seq = 0`;
- `stale = true`;
- gateway публикует его после успешного bind, чтобы consumer не ждал первого packet.

При valid packet:

- packet заменяет весь набор doors;
- `last_valid_packet_at = now`;
- `stale = false`;
- `seq += 1`;
- snapshot публикуется сразу.

При `now - last_valid_packet_at > stale_timeout`:

- если snapshot уже stale, ничего не меняется;
- иначе `stale = true`, `seq += 1`, предыдущие telemetry values сохраняются и
  публикуется один stale transition.

Строгая граница `>` сохраняет поведение Python. Монотонный clock всё равно убирает
зависимость от перевода системного времени; deterministic test проверяет отсутствие
stale event ровно на timeout и его появление сразу после timeout.

Heartbeat публикует неизменённый snapshot с текущим `ts`, не меняет `seq` и не
сбрасывает stale. `ts` означает local emit Unix epoch seconds как JSON float, а не device time;
монотонный момент не сериализуется. Если wall-clock unavailable/out of range,
gateway не публикует malformed time, пишет error и продолжает serial/FSM работу.

## ZeroMQ snapshots

Compatibility JSON остаётся compact и без schema version на migration phase:

```text
doors.state {"seq":17,"ts":1785340800.123,"doors":{"1":{"state":1,"voltage":42}},"any_open":true,"all_closed":false,"stale":false}
door.1.state {"seq":17,"ts":1785340800.123,"door_id":1,"state":1,"voltage":42,"stale":false}
```

`state` и `voltage` сохраняют текущие JSON числа. `ts` сохраняет Python contract:
Unix epoch seconds с дробной частью; это не device time. Aggregate `doors` keys
являются строками. Порядок JSON keys не часть protocol, но encoding стабилен для
fixtures.
Publisher отправляет aggregate, затем per-door snapshots по ascending `DoorId`.
Одна publish attempt failure не меняет FSM/sequence; она логируется и увеличивает
metric. PUB остаётся lossy, без ACK и persistence.

`pcrt-door-zmq` поддерживает endpoint formats, которые принимает libzmq. Для
`ipc://` transport adapter:

- создаёт parent только если это явно configured runtime directory и сначала
  берёт non-blocking exclusive lock `<instance>.lock` в том же runtime directory;
- при занятом lock второй compliant publisher завершается до попытки bind;
- перед bind pathname проверяется через `symlink_metadata`: regular file, symlink и
  directory являются fatal configuration error;
- existing socket сначала проверяется POSIX Unix-domain connect attempt, не ZMQ
  `connect`: успешное OS-level соединение означает active owner и является fatal;
  `ECONNREFUSED`/`ENOENT` после взятого lock означает stale socket, который adapter
  может удалить; остальные ошибки являются fatal и сохраняют pathname для
  диагностики;
- socket получает group-readable permissions, заданные systemd `RuntimeDirectory`/
  `RuntimeDirectoryMode`, а не world-writable `0666`;
- активный bind conflict является fatal: второй publisher не стартует и не может
  удалить endpoint первого.

Точная local group и mode устанавливаются deployment unit, не библиотекой.

## Serial lifecycle и binary

`pcrt-door-gateway` владеет `ByteSource` adapter, composition loop, metrics и
`pcrt-service` shutdown token. `GatewayEngine` владеет decoder/FSM lifecycle,
reconnect schedule и liveness transitions. `pcrt-door-zmq` владеет `ZeroMQ` publisher
и endpoint lifecycle. Production source - `SerialByteSource`. Gateway state:

```text
starting -> binding_publisher -> connecting_serial -> reading
                                       ^                |
                                       +-- backoff <----+
```

Порядок startup:

1. Parse/redact validate config.
2. Bind endpoint exclusively through `pcrt-door-zmq`.
3. Publish initial stale snapshot.
4. Open configured `ByteSource`: serial fixed port/scan в production либо Unix
   test stream при включённом test feature.
5. Read bytes, feed decoder, accept only valid packet in FSM, publish emitted snapshots.

Config fields, defaults и validation:

| Field | Default | Rule |
| --- | --- | --- |
| `door_count` | device config fallback | only `3` or `4` |
| `serial_port` | none | absolute character-device path |
| `serial_port_find` | none | only absolute glob, deterministic candidates |
| `serial_baudrate` | 19 200 | positive supported baudrate |
| `serial_bytesize` | 8 | supported serial byte size |
| `serial_parity` | `N` | one supported parity code |
| `serial_stopbits` | 1 | supported stop bit count |
| `serial_read_timeout` | 200 ms | `SERIAL_TIMEOUT`, positive seconds and no greater than heartbeat |
| `serial_reconnect_delay` | 1 s | `RECONNECT_SEC`, positive seconds |
| `stale_timeout` | 2 s | `STALE_TIMEOUT_SEC`, greater than heartbeat interval |
| `heartbeat_interval` | 500 ms | `HEARTBEAT_PUBLISH_SEC`, positive seconds |
| `zmq_endpoint` | `ipc:///run/doors.sock` | supported secure endpoint |
| `zmq_send_hwm` | 10 | positive bounded integer |

Config precedence is fixed and shared with the migration setup:

```text
defaults < config.env < door_gateway.env < environment < CLI
```

`/etc/pcrt/device.env:NUMBER_CAMS` is only a fallback for `door_count`, not an
override layer. Startup fails unless `serial_port` or `serial_port_find` is
configured. The gateway validates and logs only non-secret configuration identity;
it never logs serial bytes.

The configured fixed port is tried before discovery. If it cannot be opened and an
absolute `serial_port_find` glob is configured, candidates are tried in order.
Discovery keeps the accepted serial handle open. Every rejected candidate is closed
exactly once; accepted candidate transfers ownership to `reading`.

Read/open errors close the active source, retain cached FSM state, emit health event
and retry after `RECONNECT_SEC`. The binary uses bounded source reads; validation
requires `SERIAL_TIMEOUT <= HEARTBEAT_PUBLISH_SEC`, so a silent/reconnecting serial
input cannot prevent stale transition or heartbeat publication.

On `SIGTERM`: stop opening new ports, stop accepting new frames, publish no synthetic
closed state, close serial, close publisher, remove only transport-adapter-owned IPC
socket, and finish within systemd `TimeoutStopSec`. Cached state is intentionally not
persisted: restart begins stale/closed until fresh controller telemetry arrives.

The first deployment unit runs as dedicated `pcrt` user with serial-device group
access, `RuntimeDirectory=pcrt`, a group-readable runtime mode and a bounded
`TimeoutStopSec`. It orders before Rust recorder/processor units but does not make
them require gateway availability: consumer local expiry handles a missing gateway.
The unit enables `Restart=on-failure`, restricts filesystem writes to state/runtime
directories and allows only configured character devices. Exact hardening directives
are validated against the target distribution before enabling the unit.

## Adapter dependencies and concurrency

`pcrt-door` library has only `pcrt-model` dependency. `pcrt-door-zmq` uses `zmq`/
libzmq and owns PUB/SUB/IPC behavior. `pcrt-door-gateway` uses:

- `serialport` for configured RS-232 settings;
- `pcrt-door-zmq` for byte-compatible PUB IPC;
- `pcrt-config` for precedence/redaction and `pcrt-service` for shutdown/health.

The first binary uses synchronous bounded reads rather than making the domain library
async. `runtime` owns the active source and concrete publisher; `GatewayEngine` owns
`StreamDecoder`, FSM and timer decisions. `pcrt-door-zmq` uses non-blocking PUB sends,
so a slow subscriber or HWM exhaustion drops only a publish attempt and never changes
FSM state or source recovery.

## Consumer policy and migration

Rust gateway initially publishes the Python-compatible payload, but consumer safety
must be explicit:

- Recorder treats `stale`, missing, malformed or locally expired door data as closed.
- Processor policy is currently Python-compatible: pauses only for fresh/open
  aggregate state; stale must be classified explicitly before Rust processor exists.
  Recommended production policy is pause while stale, because stale last-closed is
  not evidence that a door is closed.
- Every Rust subscriber has a local receive-expiry timer. Gateway death after an open
  snapshot therefore cannot keep recording indefinitely.

Changing current Python processor stale behavior is a cross-service policy change,
not a silent `pcrt-door` change. It needs a deployment flag, integration test and
rollout decision.

## Metrics, health and logs

The binary exposes health through `pcrt-service` and emits structured journal logs.
Required metrics/probes:

- serial connected/disconnected and active path;
- last valid packet monotonic age;
- valid/rejected/truncated/resync/overflow packet counters;
- current `seq`, stale status and duration;
- ZeroMQ bound status and publish failures;
- reconnect attempts and selected discovery candidate.

Door values, serial bytes and endpoint credentials are not emitted in high-volume
logs. A rejected packet log includes reason and bounded hex prefix only under debug
logging.

## Fixtures and verification before code

All compatibility fixtures are immutable files under `contracts/door/`, never built
by the parser under test:

```text
contracts/door/
  frames/v1/               # binary inputs + expected packet or rejection
  streams/v1/              # chunk sequences + decode events
  snapshots/v1/            # exact one-frame ZMQ wire fixtures
  decisions.md             # legacy ambiguities and selected behavior
```

Required tests:

1. Golden 3/4-door binary packets with all raw-byte values, unordered records and
   voltage containing `0x3B`.
2. Reject prefix/length/separator/state/duplicate/missing/out-of-range IDs and
   legacy one-byte/ASCII voltage frames.
3. Test each input split boundary, garbage, header tail, concatenation, truncation,
   malformed candidate followed by valid packet, byte insertion/deletion and buffer
   overflow.
4. Deterministic FSM tests: initial stale, packet acceptance, timeout boundary,
   one-time stale event, recovery, sequence and heartbeat invariants.
5. ZMQ codec fixtures for initial stale, fresh open/closed, stale open/closed,
   aggregate/per-door topics, sequence and JSON numeric/string shape.
6. Byte-source integration without RS-232: `UnixStreamByteSource` plus test protocol
   publisher covers valid/partial/garbage/disconnect chunks, gateway FSM, ZeroMQ
   payloads, stale and heartbeat end-to-end. Fake `SerialByteSource` covers fixed-port
   errors/silence, scan failure, accepted discovery handle ownership,
   disconnect/reconnect and shutdown. PTY test is optional target-hardware coverage,
   not a local/CI prerequisite.
7. `pcrt-door-zmq` IPC ownership tests: regular file/symlink/active socket never
   unlinked; stale owned socket cleanup and single publisher bind behavior.
8. Consumer contract tests: recorder stale/local-expiry close, processor stale policy
   selected explicitly, malformed frames/messages and slow joiner behavior.
9. Shadow-run comparison harness with capture/replay of serial byte stream and emitted
   snapshots; it compares state, stale transitions and topics, allowing only stated
   timestamp-resolution differences and documented corruption-resync divergence.

## Shadow-run and cutover

Shadow-run does not bind `ipc:///run/doors.sock` and never controls recorder. It
uses one of two safe transports:

- serial fan-out hardware/serial proxy feeding Python and Rust independently; or
- recorded raw serial byte stream replayed into Rust while Python remains live.

Rust binds a separate endpoint, e.g. `ipc:///run/pcrt/doors-shadow.sock`, with a
distinct instance name. Comparator stores compact normalized observations:

```text
(input_offset, door values, stale, seq transition, topic)
```

It does not require byte-identical wall timestamps. On normal valid input a mismatch
is actionable for door values, stale edge, topic, absence of expected publish,
unexpected publish or sequence behavior. Corrupted input uses the separately
approved resync expectation above. Serial-proxy mode is required before cutover;
replay validates coverage but cannot prove live serial behavior.

Promotion gates:

1. All fixtures and integration tests pass.
2. No unexplained normalized mismatches over a defined representative route/time
   window, including reconnect and silence where feasible.
3. Operations approve the intentional changes: secure IPC ownership, monotonic stale
   clock and selected processor stale policy.
4. Systemd unit is installed but disabled; rollback command re-enables Python and
   restores its exclusive endpoint ownership.
5. Enable Rust gateway during maintenance window; monitor freshness, recorder state,
   processor pauses and publish failures; retain Python rollback until field evidence
   is sufficient.

## Explicit non-goals of first implementation

- No change to controller wire protocol or addition of CRC.
- No recording/inference/SQLite/API work in this crate.
- No remote control or persistence/replay of PUB state.
- No automatic acceptance of legacy malformed voltage encodings.
- No silent change of processor stale policy.

## Implementation status

Implemented:

- `pcrt-door`: separated `controller`, `state` and `wire` modules for fixed-offset
  packet parsing, bounded stream decoding, stale FSM and compatible snapshot encoding;
- `pcrt-door-zmq`: reusable PUB/SUB adapter, IPC ownership lifecycle and latest-state
  subscriber used by recorder;
- static 3/4-door frame, rejection, stream and snapshot fixtures;
- `pcrt-door-gateway --serial-port` and `--serial-port-find`: configurable
  baudrate/data bits/parity/stop bits/read timeout, reconnect delay and fixed-port
  discovery fallback, all feeding the same decoder/FSM path;
- real `ZeroMQ` PUB output for aggregate and per-door one-frame messages through
  `pcrt-door-zmq`;
- `SIGINT`/`SIGTERM` shutdown through `pcrt-service::ShutdownToken`, without a
  synthetic closed snapshot;
- IPC endpoint ownership lock, symlink/regular-file rejection and stale-socket removal
  only after failed local connect;
- config precedence `defaults < config.env < door_gateway.env < environment < CLI`,
  with `NUMBER_CAMS` only as the `DOOR_COUNT` fallback;
- structured health events for source connectivity, stale state, last valid packet
  age, decode counters, reconnect attempts and non-blocking publish failures;
- `test-transport` Cargo feature: `UnixStreamByteSource` and
  `pcrt-door-protocol-publisher`, which forwards scenario raw chunks unchanged;
- process-level integration test from raw fixture chunks through gateway to a real
  `ZeroMQ` subscriber, runnable without an RS-232 device.

Not yet implemented:

- progressive/exponential reconnect policy and fixed-port silence probe;
- systemd deployment hardening;
- receiver-expiry changes in recorder/processor and shadow-run comparator.
