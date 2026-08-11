use chrono::{Local, TimeZone};
use pcrt_model::ProcessingResult;
use pcrt_processing::{PreparedResult, ResultEncoder, ResultEncodingError};

pub(crate) struct TimelineResultEncoder {
    bus_id: String,
}

impl TimelineResultEncoder {
    pub(crate) fn new(bus_id: String) -> Self {
        Self { bus_id }
    }
}

impl ResultEncoder for TimelineResultEncoder {
    fn encode(&self, result: &ProcessingResult) -> Result<PreparedResult, ResultEncodingError> {
        let camera = result.camera_id.as_str().parse::<i64>().map_err(|_| {
            ResultEncodingError::new("camera ID must be an integer for timeline API")
        })?;
        let captured_at = Local
            .timestamp_millis_opt(result.captured_at_ms)
            .single()
            .ok_or_else(|| {
                ResultEncodingError::new("captured_at_ms is outside the local time range")
            })?;
        let payload_json = serde_json::json!({
            "bus": self.bus_id,
            "cam": camera,
            "date": captured_at.format("%d.%m.%YT%H:%M").to_string(),
            "in": result.counts.entered,
            "out": result.counts.exited,
        })
        .to_string();
        Ok(PreparedResult {
            idempotency_key: format!("pcrt-result:{}", result.session_id.as_str()),
            payload_json,
        })
    }
}

#[cfg(test)]
mod tests {
    use pcrt_model::{CameraId, PassengerCounts, ProcessingResult, SessionId};
    use pcrt_processing::ResultEncoder;

    use super::TimelineResultEncoder;

    #[test]
    fn produces_the_current_timeline_payload_and_stable_key() {
        let result = ProcessingResult {
            session_id: SessionId::new("cam-1-123").unwrap(),
            camera_id: CameraId::new("1").unwrap(),
            captured_at_ms: 0,
            counts: PassengerCounts {
                entered: 11,
                exited: 46,
            },
        };

        let encoded = TimelineResultEncoder::new("BUS-001".to_owned())
            .encode(&result)
            .unwrap();

        assert_eq!(encoded.idempotency_key, "pcrt-result:cam-1-123");
        let payload = serde_json::from_str::<serde_json::Value>(&encoded.payload_json).unwrap();
        assert_eq!(payload["bus"], "BUS-001");
        assert_eq!(payload["cam"], 1);
        assert_eq!(payload["in"], 11);
        assert_eq!(payload["out"], 46);
        let date = payload["date"].as_str().unwrap();
        assert_eq!(date.len(), 16);
        assert_eq!(&date[10..11], "T");
    }

    #[test]
    fn rejects_non_numeric_manifest_camera_ids() {
        let result = ProcessingResult {
            session_id: SessionId::new("cam-front-123").unwrap(),
            camera_id: CameraId::new("front").unwrap(),
            captured_at_ms: 0,
            counts: PassengerCounts::default(),
        };

        assert!(
            TimelineResultEncoder::new("BUS-001".to_owned())
                .encode(&result)
                .is_err()
        );
    }
}
