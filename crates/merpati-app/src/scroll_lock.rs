//! Trackpad scrolls stick to one axis per gesture, as in native macOS views:
//! a vertical swipe that drifts a little sideways no longer shakes text that
//! scrolls both ways (response bodies, tables, editors). Free 2D canvases opt
//! out with [`allow_free_scroll`].

use egui::{Event, MouseWheelSource, Rect, TouchPhase, Vec2};

/// Movement (points) before the gesture's axis is chosen.
const DECIDE_AFTER: f32 = 4.0;

#[derive(Default)]
pub struct AxisLock {
    /// 0 = horizontal, 1 = vertical, for the current finger gesture and its
    /// momentum.
    axis: Option<usize>,
    /// Movement of the gesture before the axis is chosen.
    pending: Vec2,
    /// The gesture started over a free-scroll area.
    free: bool,
}

/// Lets the area under `rect` (a canvas that pans in 2D) receive diagonal
/// scrolls. Call every frame the area is shown.
pub fn allow_free_scroll(ctx: &egui::Context, rect: Rect) {
    ctx.data_mut(|d| d.get_temp_mut_or_default::<FreeAreas>(egui::Id::NULL).next.push(rect));
}

#[derive(Clone, Default)]
struct FreeAreas {
    /// Shown last frame (checked against this frame's input).
    shown: Vec<Rect>,
    /// Being registered this frame.
    next: Vec<Rect>,
}

impl AxisLock {
    /// Locks the scroll events of this frame. Call from `raw_input_hook`.
    pub fn apply(&mut self, ctx: &egui::Context, input: &mut egui::RawInput) {
        let free_areas = ctx.data_mut(|d| {
            let areas = d.get_temp_mut_or_default::<FreeAreas>(egui::Id::NULL);
            areas.shown = std::mem::take(&mut areas.next);
            areas.shown.clone()
        });
        let pointer = ctx.input(|i| i.pointer.latest_pos());
        let over_free = pointer.is_some_and(|p| free_areas.iter().any(|r| r.contains(p)));
        for event in &mut input.events {
            let Event::MouseWheel { delta, phase, source, modifiers, .. } = event else { continue };
            match source {
                MouseWheelSource::Trackpad if *phase == TouchPhase::Start => {
                    *self = AxisLock { free: over_free, ..Default::default() };
                }
                MouseWheelSource::Trackpad | MouseWheelSource::Momentum => {}
                // A mouse wheel scrolls one axis anyway (⇧ for sideways), and
                // without phases (`Unknown`) a gesture's end can't be told.
                MouseWheelSource::Wheel | MouseWheelSource::Unknown => continue,
            }
            // ⇧ asks for sideways scrolling explicitly; zooms use ⌘/ctrl.
            if self.free || modifiers.shift || modifiers.command || modifiers.ctrl {
                continue;
            }
            *delta = self.lock(*delta);
        }
    }

    fn lock(&mut self, delta: Vec2) -> Vec2 {
        if self.axis.is_none() {
            self.pending += delta;
            if self.pending.length() >= DECIDE_AFTER {
                self.axis = Some(if self.pending.x.abs() > self.pending.y.abs() { 0 } else { 1 });
            }
        }
        // Until the axis is chosen, each event keeps only its larger part.
        let axis = self.axis.unwrap_or(if delta.x.abs() > delta.y.abs() { 0 } else { 1 });
        if axis == 0 { Vec2::new(delta.x, 0.0) } else { Vec2::new(0.0, delta.y) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wheel(dx: f32, dy: f32, phase: TouchPhase, source: MouseWheelSource) -> Event {
        Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: Vec2::new(dx, dy),
            phase,
            source,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn deltas(input: &egui::RawInput) -> Vec<Vec2> {
        input
            .events
            .iter()
            .filter_map(|e| match e {
                Event::MouseWheel { delta, .. } => Some(*delta),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_mostly_vertical_swipe_and_its_momentum_stay_vertical() {
        let ctx = egui::Context::default();
        let mut lock = AxisLock::default();
        let mut input = egui::RawInput {
            events: vec![
                wheel(0.5, 3.0, TouchPhase::Start, MouseWheelSource::Trackpad),
                wheel(1.5, 6.0, TouchPhase::Move, MouseWheelSource::Trackpad),
                wheel(2.0, 1.0, TouchPhase::Move, MouseWheelSource::Trackpad),
                wheel(0.0, 0.0, TouchPhase::End, MouseWheelSource::Trackpad),
                wheel(1.0, 4.0, TouchPhase::Start, MouseWheelSource::Momentum),
            ],
            ..Default::default()
        };
        lock.apply(&ctx, &mut input);
        assert!(deltas(&input).iter().all(|d| d.x == 0.0), "{:?}", deltas(&input));
        assert_eq!(deltas(&input)[1].y, 6.0);
    }

    #[test]
    fn a_new_gesture_chooses_again_and_canvases_scroll_freely() {
        let ctx = egui::Context::default();
        let mut lock = AxisLock::default();
        let mut input = egui::RawInput {
            events: vec![
                wheel(0.0, 8.0, TouchPhase::Start, MouseWheelSource::Trackpad),
                wheel(9.0, 1.0, TouchPhase::Start, MouseWheelSource::Trackpad),
                wheel(9.0, 2.0, TouchPhase::Move, MouseWheelSource::Trackpad),
            ],
            ..Default::default()
        };
        lock.apply(&ctx, &mut input);
        assert_eq!(deltas(&input)[2], Vec2::new(9.0, 0.0));

        // A gesture that starts over a free area keeps both axes.
        let mut free = AxisLock { free: true, ..Default::default() };
        let mut input = egui::RawInput {
            events: vec![wheel(5.0, 5.0, TouchPhase::Move, MouseWheelSource::Trackpad)],
            ..Default::default()
        };
        free.apply(&ctx, &mut input);
        assert_eq!(deltas(&input)[0], Vec2::new(5.0, 5.0));
    }

    #[test]
    fn mouse_wheels_are_left_alone() {
        let ctx = egui::Context::default();
        let mut lock = AxisLock::default();
        let mut input = egui::RawInput {
            events: vec![wheel(3.0, 10.0, TouchPhase::Move, MouseWheelSource::Wheel)],
            ..Default::default()
        };
        lock.apply(&ctx, &mut input);
        assert_eq!(deltas(&input)[0], Vec2::new(3.0, 10.0));
    }
}
