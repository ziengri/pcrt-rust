#![allow(
    clippy::cast_precision_loss,
    reason = "Frame dimensions are converted into the f32 coordinate space used by tracker boxes."
)]

use std::collections::{HashMap, HashSet};

use super::tracking::Track;

const DEAD_ZONE_FRACTION: f32 = 0.01;
const MIN_DEAD_ZONE_PX: f32 = 4.0;
const RECOVERY_ZONE_FRACTION: f32 = 0.12;
const MIN_RECOVERY_ZONE_PX: f32 = 24.0;
const RECOVERY_MAX_X_DISTANCE_WIDTHS: f32 = 1.5;
const RECOVERY_MAX_WIDTH_RATIO: f32 = 2.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Above,
    Below,
}

#[derive(Clone, Copy)]
struct PendingCrossing {
    from: Side,
    to: Side,
}

#[derive(Clone, Copy)]
struct TrackHistory {
    stable_side: Option<Side>,
    center_x: f32,
    center_y: f32,
    width: f32,
    class_id: i64,
    last_confirmed_frame: usize,
    pending: Option<PendingCrossing>,
}

pub(crate) struct LineCounter {
    line_y: f32,
    dead_zone: f32,
    recovery_zone: f32,
    recovery_max_gap_frames: usize,
    histories: HashMap<u64, TrackHistory>,
    entered: u64,
    exited: u64,
}

impl LineCounter {
    pub(crate) fn new(
        frame_height: usize,
        line_y_ratio: f32,
        recovery_max_gap_frames: usize,
    ) -> Self {
        Self {
            line_y: frame_height as f32 * line_y_ratio,
            dead_zone: (frame_height as f32 * DEAD_ZONE_FRACTION).max(MIN_DEAD_ZONE_PX),
            recovery_zone: (frame_height as f32 * RECOVERY_ZONE_FRACTION).max(MIN_RECOVERY_ZONE_PX),
            recovery_max_gap_frames,
            histories: HashMap::new(),
            entered: 0,
            exited: 0,
        }
    }

    pub(crate) fn entered(&self) -> u64 {
        self.entered
    }

    pub(crate) fn exited(&self) -> u64 {
        self.exited
    }

    pub(crate) fn observe(&mut self, confirmed: &[Track], predicted: &[Track], frame_index: usize) {
        self.discard_expired_histories(frame_index);
        let confirmed_ids = confirmed
            .iter()
            .map(|track| track.id)
            .collect::<HashSet<_>>();
        for track in confirmed {
            self.observe_confirmed(track, frame_index, &confirmed_ids);
        }
        for track in predicted {
            if !confirmed_ids.contains(&track.id) {
                self.observe_prediction(track);
            }
        }
    }

    fn observe_confirmed(
        &mut self,
        track: &Track,
        frame_index: usize,
        confirmed_ids: &HashSet<u64>,
    ) {
        let center_x = track.x + track.width * 0.5;
        let center_y = track.y + track.height * 0.5;
        let side = self.side(center_y);
        let previous = self.histories.get(&track.id).copied();
        let previous_side = previous.and_then(|history| history.stable_side);

        if let Some(side) = side {
            match previous_side {
                Some(previous_side) if previous_side != side => {
                    let _predicted = previous
                        .and_then(|history| history.pending)
                        .is_some_and(|pending| pending.from == previous_side && pending.to == side);
                    self.count_crossing(previous_side, side);
                }
                None if previous.is_none() => {
                    if let Some((previous_side, _predicted)) = self.recover_lost_track(
                        track.id,
                        center_x,
                        center_y,
                        track.width,
                        track.class_id,
                        side,
                        frame_index,
                        confirmed_ids,
                    ) {
                        self.count_crossing(previous_side, side);
                    }
                }
                Some(_) | None => {}
            }
        }

        self.histories.insert(
            track.id,
            TrackHistory {
                stable_side: side.or(previous_side),
                center_x,
                center_y,
                width: track.width,
                class_id: track.class_id,
                last_confirmed_frame: frame_index,
                pending: if side.is_some() {
                    None
                } else {
                    previous.and_then(|history| history.pending)
                },
            },
        );
    }

    fn observe_prediction(&mut self, track: &Track) {
        let center_y = track.y + track.height * 0.5;
        let Some(predicted_side) = self.side(center_y) else {
            return;
        };
        let Some(history) = self.histories.get_mut(&track.id) else {
            return;
        };
        let Some(stable_side) = history.stable_side else {
            return;
        };
        history.center_x = track.x + track.width * 0.5;
        history.center_y = center_y;
        history.width = track.width;
        if stable_side != predicted_side {
            history.pending = Some(PendingCrossing {
                from: stable_side,
                to: predicted_side,
            });
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn recover_lost_track(
        &mut self,
        track_id: u64,
        center_x: f32,
        center_y: f32,
        width: f32,
        class_id: i64,
        side: Side,
        frame_index: usize,
        confirmed_ids: &HashSet<u64>,
    ) -> Option<(Side, bool)> {
        let lost_id = self
            .histories
            .iter()
            .filter(|(id, history)| {
                **id != track_id
                    && !confirmed_ids.contains(*id)
                    && history.last_confirmed_frame < frame_index
                    && frame_index - history.last_confirmed_frame <= self.recovery_max_gap_frames
                    && history.class_id == class_id
                    && history
                        .stable_side
                        .is_some_and(|previous_side| previous_side != side)
                    && (history.center_y - self.line_y).abs() <= self.recovery_zone
                    && (center_y - self.line_y).abs() <= self.recovery_zone
                    && (history.center_x - center_x).abs()
                        <= history.width.max(width) * RECOVERY_MAX_X_DISTANCE_WIDTHS
                    && width >= history.width / RECOVERY_MAX_WIDTH_RATIO
                    && width <= history.width * RECOVERY_MAX_WIDTH_RATIO
            })
            .min_by(|(_, left), (_, right)| {
                let left_distance =
                    (left.center_x - center_x).abs() + (left.center_y - self.line_y).abs();
                let right_distance =
                    (right.center_x - center_x).abs() + (right.center_y - self.line_y).abs();
                left_distance.total_cmp(&right_distance)
            })
            .map(|(id, _)| *id)?;
        let history = self.histories.remove(&lost_id)?;
        Some((
            history.stable_side?,
            history.pending.is_some_and(|pending| pending.to == side),
        ))
    }

    fn count_crossing(&mut self, previous_side: Side, side: Side) {
        match (previous_side, side) {
            (Side::Above, Side::Below) => self.entered += 1,
            (Side::Below, Side::Above) => self.exited += 1,
            _ => unreachable!("line crossing sides must differ"),
        }
    }

    fn discard_expired_histories(&mut self, frame_index: usize) {
        self.histories.retain(|_, history| {
            frame_index.saturating_sub(history.last_confirmed_frame) <= self.recovery_max_gap_frames
        });
    }

    fn side(&self, y: f32) -> Option<Side> {
        if y < self.line_y - self.dead_zone {
            Some(Side::Above)
        } else if y > self.line_y + self.dead_zone {
            Some(Side::Below)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LineCounter;
    use crate::inference::tracking::Track;

    #[test]
    fn counts_each_confirmed_direction_once() {
        let mut counter = LineCounter::new(100, 0.40, 30);
        counter.observe(&[track(7, 50.0, 20.0)], &[], 0);
        counter.observe(&[track(7, 50.0, 40.0)], &[], 1);
        counter.observe(&[track(7, 50.0, 60.0)], &[], 2);
        counter.observe(&[track(7, 50.0, 70.0)], &[], 3);

        assert_eq!(counter.entered(), 1);
        assert_eq!(counter.exited(), 0);
    }

    #[test]
    fn confirms_predicted_crossing_only_after_bbox_returns() {
        let mut counter = LineCounter::new(100, 0.40, 30);
        counter.observe(&[track(7, 50.0, 20.0)], &[], 0);
        counter.observe(&[], &[track(7, 50.0, 60.0)], 1);
        assert_eq!(counter.entered(), 0);

        counter.observe(&[track(7, 50.0, 65.0)], &[], 2);
        assert_eq!(counter.entered(), 1);
    }

    #[test]
    fn recovers_id_handoff_near_line() {
        let mut counter = LineCounter::new(100, 0.40, 30);
        counter.observe(&[track(1, 50.0, 20.0)], &[], 0);
        counter.observe(&[track(2, 52.0, 60.0)], &[], 6);

        assert_eq!(counter.entered(), 1);
    }

    fn track(id: u64, center_x: f32, center_y: f32) -> Track {
        Track {
            id,
            x: center_x - 10.0,
            y: center_y - 10.0,
            width: 20.0,
            height: 20.0,
            class_id: 0,
        }
    }
}
