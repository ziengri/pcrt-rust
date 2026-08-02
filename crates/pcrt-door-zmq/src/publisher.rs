use std::{fs::File, path::PathBuf};

use crate::{DoorZmqError, DoorsState, ipc_endpoint, wire};

/// `ZeroMQ` PUB socket which exclusively owns an optional IPC endpoint.
pub struct DoorPublisher {
    _context: zmq::Context,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) socket: Option<zmq::Socket>,
    _lock: Option<File>,
    owned_ipc_path: Option<PathBuf>,
}

impl DoorPublisher {
    /// Binds a publisher with the gateway's established socket settings.
    ///
    /// IPC endpoints require an absolute path and exclusive ownership. A stale
    /// socket is removed only after acquiring the endpoint lock.
    ///
    /// # Errors
    ///
    /// Returns an error when `ZeroMQ` cannot bind or IPC ownership is unsafe.
    pub fn bind(endpoint: &str) -> Result<Self, DoorZmqError> {
        let (lock, owned_ipc_path) = ipc_endpoint::prepare(endpoint)?;
        let context = zmq::Context::new();
        let socket = context.socket(zmq::PUB).map_err(DoorZmqError::Zmq)?;
        socket.set_sndhwm(10).map_err(DoorZmqError::Zmq)?;
        socket.set_linger(0).map_err(DoorZmqError::Zmq)?;
        socket.bind(endpoint).map_err(DoorZmqError::Zmq)?;
        Ok(Self {
            _context: context,
            socket: Some(socket),
            _lock: lock,
            owned_ipc_path,
        })
    }

    /// Encodes and publishes aggregate and per-door compatibility messages.
    ///
    /// # Errors
    ///
    /// Returns a `ZeroMQ` or JSON encoding failure. The caller chooses whether to
    /// retry later; a failure never changes the supplied state.
    pub fn publish(&self, state: &DoorsState) -> Result<(), DoorZmqError> {
        let Some(socket) = &self.socket else {
            return Err(DoorZmqError::PublisherClosed);
        };
        let timestamp = wire::timestamp();
        let aggregate = wire::encode_aggregate(state, timestamp)?;
        socket
            .send(&aggregate, zmq::DONTWAIT)
            .map_err(DoorZmqError::Zmq)?;
        for (door_id, telemetry) in state.doors() {
            let frame = wire::encode_selected(*door_id, *telemetry, state, timestamp)?;
            socket
                .send(&frame, zmq::DONTWAIT)
                .map_err(DoorZmqError::Zmq)?;
        }
        Ok(())
    }

    /// Closes the socket and removes an IPC socket owned by this publisher.
    ///
    /// # Errors
    ///
    /// Returns a filesystem error while removing the owned IPC socket.
    pub fn close(mut self) -> Result<(), DoorZmqError> {
        self.socket.take();
        self.cleanup_owned_ipc_path()
    }

    fn cleanup_owned_ipc_path(&mut self) -> Result<(), DoorZmqError> {
        let Some(path) = self.owned_ipc_path.take() else {
            return Ok(());
        };
        ipc_endpoint::remove_owned(path)
    }
}

impl Drop for DoorPublisher {
    fn drop(&mut self) {
        self.socket.take();
        let _ = self.cleanup_owned_ipc_path();
    }
}
