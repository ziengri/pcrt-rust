# pcrt-door-zmq

## Назначение

`pcrt-door-zmq` является единственным Rust adapter для ZeroMQ door transport.
Он не знает RS-232 framing, stale lifecycle или recording/processing policy.

```text
pcrt-door                         pcrt-door-zmq
DoorSnapshot -> WireMessage  ->   PUB socket -> doors.state / door.N.state
                                     |
consumer <- DoorUpdate       <-   SUB socket
```

## Ownership

Crate владеет:

- `DoorPublisher`: PUB socket, `SNDHWM=10`, `LINGER=0`, non-blocking send;
- safe lifecycle `ipc://` endpoint: exclusive lock, stale socket cleanup,
  symlink/non-socket rejection and owned-socket cleanup;
- `DoorSubscriber`: exact-topic SUB socket, `RCVHWM=10` and latest valid state.

Crate не владеет:

- JSON shape и snapshot encoding: это `pcrt-door::encode_snapshot`;
- packet validation и state machine: это `pcrt-door`;
- local freshness TTL, whether an open/closed state permits recording or AI:
  это application policy consumer-а;
- serial port, reconnect loop, config or process shutdown.

## Subscriber Contract

```rust
let mut subscriber = DoorSubscriber::connect(
    "ipc:///run/doors.sock",
    DoorSubscription::Aggregate,
)?;

subscriber.drain()?;
match subscriber.latest() {
    Some(DoorUpdate::Aggregate { all_closed: true, stale: false }) => {
        // Consumer may apply its own fresh-state policy.
    }
    _ => {}
}
```

Malformed JSON, non-UTF-8 frames and unexpected topics do not replace the last
valid update. The subscriber intentionally does not assign a timestamp or TTL;
a future `AggregateDoorGate` will attach a local receive time in a separate
application layer.

## Dependency Rule

`pcrt-door-gateway`, `pcrt-recorder` and future `pcrt-processor` depend on
`pcrt-door-zmq`, not directly on `zmq`. Only this crate may create door PUB/SUB
sockets or own `/run/doors.sock`.
