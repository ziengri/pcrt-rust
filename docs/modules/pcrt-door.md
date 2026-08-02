# Door Gateway Architecture

## Purpose

`pcrt-door-gateway` is a private production component. It reads controller bytes
from RS-232, maintains the live door state and publishes it to the shared door bus.

```text
SerialSource -> protocol -> state -> service -> pcrt-door-zmq::DoorPublisher
```

The gateway is the only component that knows the controller packet format. Recorder
and processor never import its protocol, decoder or state machine.

## Ownership

```text
pcrt-model
  DoorId, DoorState, DoorTelemetry

pcrt-door-zmq
  DoorsState, ReceivedDoorsState
  topics, JSON codec, PUB/SUB, IPC endpoint ownership

pcrt-door-gateway
  serial source, controller protocol, state lifecycle
  reconnect/liveness/heartbeat policy, health and runtime
```

`DoorsState` belongs to `pcrt-door-zmq`: it is the public contract exchanged by
the gateway, recorder and processor. It contains a complete door map, `sequence`
and `stale`; it has no serial bytes, endpoint, JSON, topic or clock details.

`any_open` and `all_closed` are derived from the complete door map. They are not
independently mutable state. The compatible JSON fields remain on the wire and are
validated against the derived values when a message is decoded.

The former `pcrt-door` crate was removed. Controller protocol and state lifecycle
are private gateway code; no shared API exposes them.

## Gateway Layout

```text
bins/pcrt-door-gateway/src/
|-- main.rs                 # parse -> construct -> run
|-- config/
|   |-- mod.rs
|   |-- args.rs             # CLI parsing and semantic validation
|   `-- env.rs              # config.env, service env and process environment
|-- door/                   # private to this binary
|   |-- mod.rs
|   |-- service.rs          # coordinates protocol, state and timer policy
|   |-- effect.rs           # service outputs for runtime to execute
|   |-- health.rs           # gateway operational counters
|   |-- source/
|   |   |-- mod.rs
|   |   |-- serial.rs       # serialport setup and glob discovery
|   |   `-- test_unix.rs    # feature-gated integration byte source
|   |-- protocol/
|   |   |-- mod.rs
|   |   |-- packet.rs       # fixed controller packet validation
|   |   |-- stream.rs       # bounded framing and resynchronization
|   |   `-- error.rs
|   `-- state/
|       |-- mod.rs
|       `-- machine.rs      # sequence and stale lifecycle
|-- runtime.rs              # blocking reads, shutdown and effect execution
`-- shutdown.rs             # SIGINT/SIGTERM installation, if it merits a file
```

This is the implemented layout. Files are created only when their responsibility
exists; an empty module directory is not useful.

### Source

`door::source` reads raw bytes and opens a configured device. The production
implementation is `SerialSource`; it owns serial settings and discovery fallback.
The feature-gated Unix source exists only for end-to-end tests.

The source does not recognize packet headers, join reads into packets, calculate
staleness or publish messages. A single source does not justify a custom trait:
the runtime uses concrete sources through `std::io::Read`. A source trait is added
only when a second production source requires substitutability.

### Protocol

`door::protocol` converts arbitrary byte chunks into validated private
`ControllerPacket` values.

```text
raw bytes -> StreamDecoder -> DecodeEvent -> ControllerPacket
```

It owns `!DOORS:` framing, fixed record offsets, configured door count, raw voltage
bytes, malformed-candidate handling, partial frames and bounded resynchronization.
It does not know serial devices, ZMQ, heartbeats or consumer policy.

### State

`door::state` converts validated controller packets into shared `DoorsState`.

```text
ControllerPacket + Instant -> DoorsState
```

It owns initial stale state, sequence changes, full-state replacement, stale timeout
and recovery after a fresh packet. It does not know topics, JSON, sockets or wall
clock timestamps.

### Service

`door::service::DoorService` is the private orchestration unit, equivalent in role
to the Python `SessionRecorderService`. It owns the decoder, state machine, heartbeat
deadline, reconnect schedule, serial liveness policy and `GatewayHealth`.

```text
bytes + monotonic time + connection events -> GatewayEffect values
```

It returns effects rather than opening serial devices or calling ZMQ directly.

```rust
enum GatewayEffect {
    Publish(DoorsState),
    DisconnectSource,
    PacketRejected(ProtocolError),
    PacketTruncated,
    DecoderOverflow,
    HealthChanged(GatewayHealth),
}
```

Concrete names may differ, but the direction does not: service decides, runtime
executes.

### Runtime

`runtime.rs` constructs `SerialSource`, `DoorService` and `DoorPublisher`. It passes
read chunks and `Instant::now()` to the service, executes its effects, logs failures
and stops on `SIGINT`/`SIGTERM`.

Runtime does not parse controller packets or decide state transitions. `main.rs`
contains only process exit handling around `config::parse_args` and `runtime::run`.

## Shared Door Bus

```text
private gateway state machine
    -> pcrt-door-zmq::DoorsState
    -> DoorPublisher::publish(&state)
    -> doors.state and door.N.state
    -> AggregateDoorSubscriber
    -> ReceivedDoorsState
    -> private recorder or processor DoorGate
```

`DoorPublisher::publish(&DoorsState)` creates the compatible JSON and supplies the
wall-clock `ts` itself at publish time. `ts` is transport metadata, not a gateway
state-machine input.

`AggregateDoorSubscriber` retains the latest valid aggregate state and records when
it was received. It does not decide whether that state is fresh enough to start
work. That decision stays private to each consumer:

```text
recorder DoorGate: selected configured door is locally fresh and open
processor DoorGate: aggregate state is locally fresh and all doors are closed
```

Door state is live transport input. It is never persisted in session storage as a
handoff from recorder to processor.

## Compatibility Contract

The refactor does not change deployment-visible behavior:

| Area | Contract |
| --- | --- |
| Serial prefix | ASCII `!DOORS:` |
| Door count | exactly 3 or 4, IDs `1..=count` |
| Packet record | `<id>=<state>,<voltage>;`, exactly 6 bytes |
| Controller state | raw byte `0` closed, `1` open |
| Aggregate topic | `doors.state` |
| Per-door topic | `door.N.state` |
| Wire frame | `<topic><space><compact JSON>` UTF-8 frame |
| Aggregate fields | `seq`, `ts`, `doors`, `any_open`, `all_closed`, `stale` |
| Default endpoint | `ipc:///run/doors.sock` |
| Startup | publish an initial stale, all-closed state after bind |
| Configuration | CLI/env names, precedence and default timings remain unchanged |

Publisher sends the aggregate frame first, then per-door frames in ascending door ID.
PUB remains lossy: a publish failure is logged and counted but never changes sequence
or door state.

## Dependency Direction

```text
pcrt-model
    ^
    |
pcrt-door-zmq
    ^
    +-- pcrt-door-gateway
    +-- pcrt-recorder
    `-- pcrt-processor
```

- `pcrt-model` has no transport, filesystem, serial, JSON or time policy.
- `pcrt-door-zmq` has no controller protocol, serial source, stale/reconnect policy,
  recording or processing policy.
- `pcrt-door-gateway` is the only owner of controller protocol and serial lifecycle.
- `pcrt-recorder` and `pcrt-processor` use shared door messages but never depend on
  the gateway binary.

## Verification

Keep fixtures under `contracts/door/` independent from the implementation:

```text
frames/    raw complete controller packets
streams/   raw chunking, corruption and disconnect scenarios
snapshots/ aggregate and per-door wire examples
```

Required coverage follows the component boundaries:

1. Protocol tests cover valid, malformed, partial, concatenated and resynchronized
   controller byte streams.
2. State tests cover initial stale state, sequence, stale boundary, heartbeat and
   recovery with supplied monotonic times.
3. Service tests cover bytes/time/connection events to effects without serial or ZMQ.
4. `pcrt-door-zmq` tests cover JSON/topic validation, IPC ownership and latest valid
   aggregate state.
5. Feature-gated process integration tests cover Unix byte source through real ZMQ
   PUB/SUB without an RS-232 device.
