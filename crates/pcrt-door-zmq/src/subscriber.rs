use std::time::Instant;

use crate::{DoorZmqError, ReceivedDoorsState, wire};

/// `ZeroMQ` SUB socket retaining only the latest valid aggregate state.
pub struct AggregateDoorSubscriber {
    _context: zmq::Context,
    socket: zmq::Socket,
    latest: Option<ReceivedDoorsState>,
}

impl AggregateDoorSubscriber {
    /// Connects to the aggregate `doors.state` topic.
    ///
    /// # Errors
    ///
    /// Returns a `ZeroMQ` socket setup or connect failure.
    pub fn connect(endpoint: &str) -> Result<Self, DoorZmqError> {
        Self::connect_with_context(zmq::Context::new(), endpoint)
    }

    #[cfg(test)]
    pub(crate) fn connect_with_context(
        context: zmq::Context,
        endpoint: &str,
    ) -> Result<Self, DoorZmqError> {
        Self::connect_inner(context, endpoint)
    }

    #[cfg(not(test))]
    fn connect_with_context(context: zmq::Context, endpoint: &str) -> Result<Self, DoorZmqError> {
        Self::connect_inner(context, endpoint)
    }

    fn connect_inner(context: zmq::Context, endpoint: &str) -> Result<Self, DoorZmqError> {
        let socket = context.socket(zmq::SUB).map_err(DoorZmqError::Zmq)?;
        socket
            .set_subscribe(b"doors.state")
            .map_err(DoorZmqError::Zmq)?;
        socket.set_rcvhwm(10).map_err(DoorZmqError::Zmq)?;
        socket.connect(endpoint).map_err(DoorZmqError::Zmq)?;
        Ok(Self {
            _context: context,
            socket,
            latest: None,
        })
    }

    /// Drains currently available frames and retains the latest valid update.
    ///
    /// Invalid UTF-8, wrong topic and malformed payloads are ignored without
    /// replacing the last valid update. `EAGAIN` means the socket is drained.
    ///
    /// # Errors
    ///
    /// Returns an unexpected `ZeroMQ` receive error.
    pub fn drain(&mut self) -> Result<(), DoorZmqError> {
        loop {
            let frame = match self.socket.recv_string(zmq::DONTWAIT) {
                Ok(Ok(frame)) => frame,
                Ok(Err(_)) => continue,
                Err(zmq::Error::EAGAIN) => return Ok(()),
                Err(error) => return Err(DoorZmqError::Zmq(error)),
            };
            let Some((topic, payload)) = frame.split_once(' ') else {
                continue;
            };
            if topic != "doors.state" {
                continue;
            }
            if let Some(state) = wire::decode_aggregate(payload) {
                self.latest = Some(ReceivedDoorsState {
                    state,
                    received_at: Instant::now(),
                });
            }
        }
    }

    /// Returns the latest valid update without applying any freshness policy.
    #[must_use]
    pub const fn latest(&self) -> Option<&ReceivedDoorsState> {
        self.latest.as_ref()
    }
}
