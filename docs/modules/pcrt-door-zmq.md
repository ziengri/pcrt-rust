# pcrt-door-zmq

## Назначение

`pcrt-door-zmq` является общим Rust building block для door-message bus. Он
владеет публичным состоянием дверей, его `ZeroMQ` representation и безопасным
IPC transport. Он не знает RS-232 framing, gateway stale lifecycle или
recording/processing policy.

```text
pcrt-door-gateway private              pcrt-door-zmq shared
private controller/FSM -> DoorsState -> PUB socket -> doors.state / door.N.state
                                              |
recorder / processor private <- ReceivedDoorsState <- SUB socket
```

## Ownership

Crate владеет:

- `DoorsState`: shared aggregate contract между gateway, recorder и processor;
- `ReceivedDoorsState`: `DoorsState` и локальный момент, когда subscriber его
  получил;
- topic mapping `doors.state` / `door.N.state`, compact JSON schema, encode/decode
  and validation;
- publish timestamp `ts`: publisher получает wall-clock непосредственно перед
  отправкой, поэтому gateway не передаёт timestamp вручную;
- `DoorPublisher`: PUB socket, `SNDHWM=10`, `LINGER=0`, non-blocking send;
- safe lifecycle `ipc://` endpoint: exclusive lock, stale socket cleanup,
  symlink/non-socket rejection and owned-socket cleanup;
- `AggregateDoorSubscriber`: SUB socket, `RCVHWM=10` and latest valid aggregate
  `DoorsState`.

Crate не владеет:

- базовыми value types `DoorId`, `DoorState`, `DoorTelemetry`: это `pcrt-model`;
- packet validation, raw controller packet и state machine: это private logic
  `pcrt-door-gateway`;
- local freshness TTL, whether an open/closed state permits recording or AI:
  это application policy consumer-а;
- serial port, reconnect loop, config or process shutdown.

`DoorsState` не хранит transport details, serial bytes, endpoint, JSON, `Instant`
или IPC ownership. Он содержит только `sequence`, complete map `DoorId ->
DoorTelemetry` и `stale`. `any_open` и `all_closed` являются derived methods из
полного `doors`, а не independently mutable fields.

JSON `any_open` и `all_closed` сохраняются для wire compatibility, но при decode
сверяются с вычислением из `DoorsState`; contradictory message отклоняется.

## Public API

Публичный API не передаёт consumers технические frame/JSON DTO:

```rust
pub struct DoorsState { /* sequence, complete doors, stale */ }
pub struct ReceivedDoorsState { /* DoorsState + local received_at */ }

impl DoorPublisher {
    pub fn publish(&self, state: &DoorsState) -> Result<(), DoorZmqError>;
}

impl AggregateDoorSubscriber {
    pub fn connect(endpoint: &str) -> Result<Self, DoorZmqError>;
    pub fn drain(&mut self) -> Result<(), DoorZmqError>;
    pub fn latest(&self) -> Option<&ReceivedDoorsState>;
}
```

Internal JSON payload structs and `<topic><space><JSON>` frames remain private
implementation details of this crate. Per-door topics continue to publish for
external compatibility, while Rust recorder and processor consume aggregate
`doors.state` and choose their own policy from the full shared state.

## Subscriber Contract

```rust
let mut subscriber = AggregateDoorSubscriber::connect("ipc:///run/doors.sock")?;

subscriber.drain()?;
match subscriber.latest() {
    Some(received) if !received.state().stale() && received.state().all_closed() => {
        // Consumer may apply its own fresh-state policy.
    }
    _ => {}
}
```

Malformed JSON, non-UTF-8 frames, unexpected topics and contradictory derived
flags do not replace the last valid update. Subscriber records `received_at`, but
does not apply TTL. Recorder and processor own separate private `DoorGate` policy:
the recorder checks its selected `DoorId`; processor permits a new claim only when
the aggregate is fresh and all doors are closed.

## Dependency Rule

`pcrt-door-gateway`, `pcrt-recorder` and future `pcrt-processor` depend on
`pcrt-door-zmq`, not directly on `zmq`. Only this crate may create door PUB/SUB
sockets or own `/run/doors.sock`.
