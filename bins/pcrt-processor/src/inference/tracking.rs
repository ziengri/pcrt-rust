use std::collections::{HashMap, HashSet};

use trackforge::{
    trackers::byte_track::{ByteTrack, STrack, TrackState},
    utils::kalman::KalmanFilter,
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Detection {
    pub(crate) x1: f32,
    pub(crate) y1: f32,
    pub(crate) x2: f32,
    pub(crate) y2: f32,
    pub(crate) confidence: f32,
    pub(crate) class_id: i64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Track {
    pub(crate) id: u64,
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) class_id: i64,
}

pub(crate) struct TrackerUpdate {
    pub(crate) confirmed: Vec<Track>,
    pub(crate) predicted: Vec<Track>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TrackerConfig {
    pub(crate) track_threshold: f32,
    pub(crate) track_buffer: usize,
    pub(crate) match_threshold: f32,
    pub(crate) init_threshold: f32,
}

struct StoredTrack {
    track: STrack,
    missed_frames: usize,
}

pub(crate) struct Tracker {
    byte_track: ByteTrack,
    lost_track_buffer: usize,
    kalman_filter: KalmanFilter,
    tracks: HashMap<u64, StoredTrack>,
}

impl Tracker {
    pub(crate) fn new(config: TrackerConfig) -> Self {
        Self {
            byte_track: ByteTrack::new(
                config.track_threshold,
                config.track_buffer,
                config.match_threshold,
                config.init_threshold,
            ),
            lost_track_buffer: config.track_buffer,
            kalman_filter: KalmanFilter::default(),
            tracks: HashMap::new(),
        }
    }

    pub(crate) fn update(&mut self, detections: &[Detection]) -> TrackerUpdate {
        let confirmed_tracks = self.byte_track.update(detections_to_tlwh(detections));
        let confirmed_ids = confirmed_tracks
            .iter()
            .map(|track| track.track_id)
            .collect::<HashSet<_>>();
        let mut predicted = Vec::new();

        self.tracks.retain(|track_id, stored| {
            if confirmed_ids.contains(track_id) {
                return false;
            }

            let mut predicted_track = stored.track.clone();
            if stored.missed_frames > 0 {
                predicted_track.state = TrackState::Lost;
            }
            predicted_track.predict(&self.kalman_filter);
            predicted_track.state = TrackState::Lost;
            stored.track = predicted_track.clone();
            stored.missed_frames += 1;
            if stored.missed_frames > self.lost_track_buffer {
                return false;
            }
            predicted.push(track_from_byte_track(&predicted_track));
            true
        });

        let confirmed = confirmed_tracks.iter().map(track_from_byte_track).collect();
        for track in confirmed_tracks {
            self.tracks.insert(
                track.track_id,
                StoredTrack {
                    track,
                    missed_frames: 0,
                },
            );
        }

        TrackerUpdate {
            confirmed,
            predicted,
        }
    }
}

fn detections_to_tlwh(detections: &[Detection]) -> Vec<([f32; 4], f32, i64)> {
    detections
        .iter()
        .map(|detection| {
            (
                [
                    detection.x1,
                    detection.y1,
                    detection.x2 - detection.x1,
                    detection.y2 - detection.y1,
                ],
                detection.confidence,
                detection.class_id,
            )
        })
        .collect()
}

fn track_from_byte_track(track: &STrack) -> Track {
    Track {
        id: track.track_id,
        x: track.tlwh[0],
        y: track.tlwh[1],
        width: track.tlwh[2],
        height: track.tlwh[3],
        class_id: track.class_id,
    }
}

#[cfg(test)]
mod tests {
    use super::{Detection, Tracker, TrackerConfig};

    #[test]
    fn preserves_id_for_nearby_detection() {
        let mut tracker = Tracker::new(config());
        let first = tracker.update(&[detection(10.0, 10.0)]);
        let second = tracker.update(&[detection(12.0, 12.0)]);

        assert_eq!(first.confirmed.len(), 1);
        assert_eq!(second.confirmed.len(), 1);
        assert_eq!(first.confirmed[0].id, second.confirmed[0].id);
    }

    #[test]
    fn exposes_kalman_predictions_for_lost_tracks() {
        let mut tracker = Tracker::new(config());
        let first = tracker.update(&[detection(10.0, 10.0)]);
        let lost = tracker.update(&[]);

        assert_eq!(first.confirmed.len(), 1);
        assert!(lost.confirmed.is_empty());
        assert_eq!(lost.predicted.len(), 1);
        assert_eq!(lost.predicted[0].id, first.confirmed[0].id);
    }

    #[test]
    fn drops_lost_track_after_buffer_expires() {
        let mut tracker = Tracker::new(TrackerConfig {
            track_buffer: 1,
            ..config()
        });
        tracker.update(&[detection(10.0, 10.0)]);

        assert_eq!(tracker.update(&[]).predicted.len(), 1);
        assert!(tracker.update(&[]).predicted.is_empty());
    }

    fn config() -> TrackerConfig {
        TrackerConfig {
            track_threshold: 0.5,
            track_buffer: 30,
            match_threshold: 0.8,
            init_threshold: 0.6,
        }
    }

    fn detection(x1: f32, y1: f32) -> Detection {
        Detection {
            x1,
            y1,
            x2: x1 + 50.0,
            y2: y1 + 100.0,
            confidence: 0.9,
            class_id: 0,
        }
    }
}
