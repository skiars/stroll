use std::time::Duration;

const NAVIGATION_SECONDS: f32 = 0.140;

/// Cubic scroll motion for one axis. Targets and positions are in logical pixels.
#[derive(Clone, Copy)]
pub(crate) struct NavigationAxis {
    start: f32,
    target: f32,
    duration: f32,
    elapsed: Duration,
    initial_slope: f32,
}

impl NavigationAxis {
    pub(crate) fn new(start: f32, target: f32) -> Self {
        Self {
            start,
            target,
            duration: NAVIGATION_SECONDS,
            elapsed: Duration::ZERO,
            initial_slope: 3.,
        }
    }

    pub(crate) fn retarget(&mut self, current: f32, target: f32) {
        if target == self.target {
            return;
        }

        let velocity = self.velocity();
        let distance = target - current;
        *self = Self::new(current, target);
        if distance * velocity > 0. {
            // Inherit the outgoing velocity. A normalized tangent of at most
            // three keeps this cubic monotonic with zero velocity at arrival.
            // A closer target may need a shorter curve to avoid overshooting.
            self.duration = self.duration.min(3. * (distance / velocity));
            self.initial_slope = (velocity * self.duration / distance).min(3.);
        }
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.start == self.target || self.elapsed.as_secs_f32() >= self.duration
    }

    pub(crate) fn velocity(&self) -> f32 {
        if self.is_finished() {
            return 0.;
        }
        let t = self.elapsed.as_secs_f32() / self.duration;
        let slope = (1. - t) * (self.initial_slope + (6. - 3. * self.initial_slope) * t);
        (self.target - self.start) / self.duration * slope
    }

    pub(crate) fn advance(&mut self, elapsed: Duration) -> f32 {
        self.elapsed += elapsed;
        if self.is_finished() {
            return self.target;
        }
        if self.elapsed.is_zero() {
            return self.start;
        }
        let t = self.elapsed.as_secs_f32() / self.duration;
        let remaining = (1. - t).powi(2) * (1. + (2. - self.initial_slope) * t);
        self.target - (self.target - self.start) * remaining
    }
}
