//! Deterministic cursor path engine.
//!
//! The engine consumes post-acceleration desktop displacement produced by the
//! operating system, keeps a bounded history of native positions and produces
//! interpolated positions on a 1000 Hz schedule. It never extrapolates past the
//! newest real sample: at worst the cursor trails the physical pointer by the
//! frozen burst lag.

/// Bounded native-sample history. 64 samples cover more than 500 ms at 125 Hz.
pub const MAX_SAMPLES: usize = 64;
/// Bounded moving-interval history used for the burst lag.
pub const MAX_INTERVALS: usize = 8;
/// Shortest interval treated as real mouse motion (1 kHz equivalent).
pub const MIN_INTERVAL_NS: u64 = 1_000_000;
/// Longest interval treated as continuous motion.
pub const MAX_INTERVAL_NS: u64 = 40_000_000;
/// A gap at or above this value separates two bursts.
pub const BURST_GAP_NS: u64 = 100_000_000;
/// Lower clamp for the deliberate buffering lag.
pub const MIN_LAG_NS: u64 = 4_000_000;
/// Upper clamp for the deliberate buffering lag.
pub const MAX_LAG_NS: u64 = 40_000_000;
/// Native moves passed through before suppression starts after a (re)anchor.
pub const WARMUP_MOVES: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// A coordinate or displacement was NaN or infinite.
    NonFinite,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl std::ops::Add for Point {
    type Output = Point;

    fn add(self, other: Point) -> Point {
        Point::new(self.x + other.x, self.y + other.y)
    }
}

impl std::ops::Sub for Point {
    type Output = Point;

    fn sub(self, other: Point) -> Point {
        Point::new(self.x - other.x, self.y - other.y)
    }
}

impl Point {
    pub const ZERO: Point = Point { x: 0.0, y: 0.0 };

    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }

    #[allow(clippy::should_implement_trait)]
    pub fn add(self, other: Point) -> Point {
        self + other
    }

    #[allow(clippy::should_implement_trait)]
    pub fn sub(self, other: Point) -> Point {
        self - other
    }

    pub fn scale(self, factor: f64) -> Point {
        Point::new(self.x * factor, self.y * factor)
    }

    pub fn distance(self, other: Point) -> f64 {
        let dx = self.x - other.x;
        let dy = self.y - other.y;
        (dx * dx + dy * dy).sqrt()
    }

    pub fn lerp(from: Point, to: Point, t: f64) -> Point {
        Point::new(from.x + (to.x - from.x) * t, from.y + (to.y - from.y) * t)
    }
}

/// Physical pixel grid of the virtual desktop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PixelGrid {
    pub origin: Point,
    pub size: Point,
}

impl PixelGrid {
    pub fn new(origin: Point, size: Point) -> Self {
        Self { origin, size }
    }

    /// Rounds to whole physical pixels and clamps inside the desktop.
    pub fn quantize(&self, point: Point) -> (i32, i32) {
        let max_x = self.origin.x + self.size.x - 1.0;
        let max_y = self.origin.y + self.size.y - 1.0;
        let x = point.x.round().clamp(self.origin.x, max_x);
        let y = point.y.round().clamp(self.origin.y, max_y);
        (x as i32, y as i32)
    }
}

#[derive(Clone, Copy, Debug)]
struct Sample {
    at_ns: u64,
    pos: Point,
}

impl Sample {
    const ZERO: Sample = Sample {
        at_ns: 0,
        pos: Point::ZERO,
    };
}

/// Bounded interpolation engine; no allocation on any path.
pub struct Engine {
    samples: [Sample; MAX_SAMPLES],
    start: usize,
    len: usize,
    intervals: [u64; MAX_INTERVALS],
    interval_count: usize,
    interval_next: usize,
    last: Option<Sample>,
    anchor: Point,
    lag_ns: Option<u64>,
    warmup_left: u8,
    burst_open: bool,
    endpoint_emitted: bool,
    floor_ns: Option<u64>,
}

impl Engine {
    pub fn new(anchor: Point) -> Self {
        Self {
            samples: [Sample::ZERO; MAX_SAMPLES],
            start: 0,
            len: 0,
            intervals: [0; MAX_INTERVALS],
            interval_count: 0,
            interval_next: 0,
            last: None,
            anchor,
            lag_ns: None,
            warmup_left: WARMUP_MOVES,
            burst_open: false,
            endpoint_emitted: true,
            floor_ns: None,
        }
    }

    /// Current anchor used when no native sample has arrived yet.
    pub fn anchor(&self) -> Point {
        self.last.map_or(self.anchor, |s| s.pos)
    }

    /// Frozen buffering lag for the current burst, once learned.
    pub fn lag_ns(&self) -> Option<u64> {
        self.lag_ns
    }

    pub fn warmup_left(&self) -> u8 {
        self.warmup_left
    }

    pub fn is_warmed(&self) -> bool {
        self.warmup_left == 0
    }

    /// A target still has to be emitted for the newest native sample.
    pub fn has_pending(&self) -> bool {
        self.len > 0 && !self.endpoint_emitted
    }

    /// Newest authoritative native position, if any.
    pub fn native_position(&self) -> Point {
        self.newest().map_or(self.anchor, |s| s.pos)
    }

    /// Applies OS-produced displacement.
    pub fn push_native(&mut self, at_ns: u64, delta: Point) -> Result<(), Fault> {
        if !delta.is_finite() {
            return Err(Fault::NonFinite);
        }
        if delta.x == 0.0 && delta.y == 0.0 {
            return Ok(());
        }
        let position = self.anchor().add(delta);
        self.push_position(at_ns, position)
    }

    /// Applies an authoritative absolute position (absolute-mapping adapters).
    pub fn push_absolute(&mut self, at_ns: u64, position: Point) -> Result<(), Fault> {
        if !position.is_finite() {
            return Err(Fault::NonFinite);
        }
        let delta = position.sub(self.anchor());
        if delta.x == 0.0 && delta.y == 0.0 {
            return Ok(());
        }
        self.push_position(at_ns, position)
    }

    fn push_position(&mut self, at_ns: u64, position: Point) -> Result<(), Fault> {
        if !position.is_finite() {
            return Err(Fault::NonFinite);
        }
        let mut effective = at_ns;
        let mut interval = None;
        let mut gap = self.last.is_none();
        if let Some(last) = self.last {
            if at_ns > last.at_ns {
                let dt = at_ns - last.at_ns;
                if dt >= BURST_GAP_NS {
                    gap = true;
                } else {
                    interval = Some(dt);
                }
            } else {
                // Coincident or backward timestamp: keep the path monotonic
                // without inventing a measurable interval.
                effective = last.at_ns.saturating_add(1);
            }
        }
        if gap {
            self.burst_open = false;
        }
        if let Some(dt) = interval
            && (MIN_INTERVAL_NS..=MAX_INTERVAL_NS).contains(&dt)
        {
            let opening = !self.burst_open;
            self.record_interval(dt);
            self.burst_open = true;
            if opening || self.lag_ns.is_none() {
                self.lag_ns = Some(self.lag_from_history());
            }
        }
        self.append(Sample {
            at_ns: effective,
            pos: position,
        });
        self.last = Some(Sample {
            at_ns: effective,
            pos: position,
        });
        self.endpoint_emitted = false;
        if self.warmup_left > 0 {
            self.warmup_left -= 1;
            if self.warmup_left == 0 {
                if self.lag_ns.is_none() {
                    // No usable interval evidence yet: buffer the minimum.
                    self.lag_ns = Some(MIN_LAG_NS);
                }
                self.floor_ns = Some(effective);
            }
        }
        Ok(())
    }

    /// Produces the interpolated position for this schedule instant.
    ///
    /// Returns `None` once the exact endpoint has been produced, which lets the
    /// caller stop its timer instead of polling.
    pub fn sample(&mut self, at_ns: u64) -> Option<Point> {
        if self.endpoint_emitted {
            return None;
        }
        let latest = self.newest()?;
        if self.warmup_left > 0 {
            return Some(latest.pos);
        }
        let lag = self.lag_ns.unwrap_or(MIN_LAG_NS);
        let mut t = at_ns.saturating_sub(lag);
        if let Some(floor) = self.floor_ns {
            t = t.max(floor);
        }
        if t >= latest.at_ns {
            self.endpoint_emitted = true;
            return Some(latest.pos);
        }
        let first = *self.sample_at(0);
        if t <= first.at_ns {
            return Some(first.pos);
        }
        let mut index = 1;
        while index < self.len {
            let current = *self.sample_at(index);
            if current.at_ns >= t {
                let previous = *self.sample_at(index - 1);
                let span = current.at_ns - previous.at_ns;
                if span == 0 {
                    return Some(current.pos);
                }
                let fraction = (t - previous.at_ns) as f64 / span as f64;
                return Some(Point::lerp(previous.pos, current.pos, fraction));
            }
            index += 1;
        }
        Some(latest.pos)
    }

    /// Drops buffered movement and restarts warmup at `anchor`.
    ///
    /// Learned intervals survive: they describe the device, not this gesture.
    pub fn discard_and_reanchor(&mut self, at_ns: u64, anchor: Point) {
        self.reset_ring();
        self.last = Some(Sample { at_ns, pos: anchor });
        self.anchor = anchor;
        self.warmup_left = WARMUP_MOVES;
        self.burst_open = false;
        self.endpoint_emitted = true;
        self.floor_ns = None;
    }

    /// Full reset after sleep, topology change or a latched fault.
    pub fn reset_history(&mut self, at_ns: u64, anchor: Point) {
        self.intervals = [0; MAX_INTERVALS];
        self.interval_count = 0;
        self.interval_next = 0;
        self.lag_ns = None;
        self.discard_and_reanchor(at_ns, anchor);
        self.last = None;
        self.warmup_left = WARMUP_MOVES;
        self.burst_open = false;
    }

    fn reset_ring(&mut self) {
        self.start = 0;
        self.len = 0;
    }

    fn append(&mut self, sample: Sample) {
        if self.len < MAX_SAMPLES {
            let index = (self.start + self.len) % MAX_SAMPLES;
            self.samples[index] = sample;
            self.len += 1;
        } else {
            self.samples[self.start] = sample;
            self.start = (self.start + 1) % MAX_SAMPLES;
        }
    }

    fn sample_at(&self, index: usize) -> &Sample {
        &self.samples[(self.start + index) % MAX_SAMPLES]
    }

    fn newest(&self) -> Option<Sample> {
        if self.len == 0 {
            None
        } else {
            Some(*self.sample_at(self.len - 1))
        }
    }

    fn record_interval(&mut self, dt: u64) {
        self.intervals[self.interval_next] = dt;
        self.interval_next = (self.interval_next + 1) % MAX_INTERVALS;
        if self.interval_count < MAX_INTERVALS {
            self.interval_count += 1;
        }
    }

    fn lag_from_history(&self) -> u64 {
        match self.median_interval() {
            Some(median) => {
                let scaled = median.saturating_mul(3) / 2;
                scaled.clamp(MIN_LAG_NS, MAX_LAG_NS)
            }
            None => MIN_LAG_NS,
        }
    }

    fn median_interval(&self) -> Option<u64> {
        if self.interval_count == 0 {
            return None;
        }
        let mut sorted = [0_u64; MAX_INTERVALS];
        let first = (self.interval_next + MAX_INTERVALS - self.interval_count) % MAX_INTERVALS;
        for (index, slot) in sorted.iter_mut().take(self.interval_count).enumerate() {
            *slot = self.intervals[(first + index) % MAX_INTERVALS];
        }
        let slice = &mut sorted[..self.interval_count];
        slice.sort_unstable();
        let count = slice.len();
        Some(if count % 2 == 1 {
            slice[count / 2]
        } else {
            (slice[count / 2 - 1] + slice[count / 2]) / 2
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(engine: &mut Engine, start_ns: u64, points: &[(u64, Point)]) -> u64 {
        let mut time = start_ns;
        let mut previous = engine.anchor();
        for (dt_ns, point) in points {
            time += *dt_ns;
            let delta = point.sub(previous);
            engine.push_native(time, delta).expect("finite input");
            previous = *point;
        }
        time
    }

    fn steady_trace(
        count: usize,
        step: Point,
        interval_ns: u64,
        start: Point,
    ) -> Vec<(u64, Point)> {
        let mut points = Vec::with_capacity(count);
        for index in 0..count {
            let n = index as f64 + 1.0;
            points.push((
                interval_ns,
                Point::new(start.x + step.x * n, start.y + step.y * n),
            ));
        }
        points
    }

    #[test]
    fn first_move_passes_through_without_inventing_motion() {
        let mut engine = Engine::new(Point::new(100.0, 100.0));
        engine
            .push_native(10_000_000, Point::new(5.0, 0.0))
            .unwrap();
        assert_eq!(engine.warmup_left(), WARMUP_MOVES - 1);
        assert_eq!(engine.sample(10_000_000), Some(Point::new(105.0, 100.0)));
    }

    #[test]
    fn endpoint_is_exact_and_then_idle() {
        let mut engine = Engine::new(Point::ZERO);
        let trace = steady_trace(6, Point::new(4.0, 1.0), 8_000_000, Point::ZERO);
        let mut time = 1_000_000_000;
        time = feed(&mut engine, time, &trace);
        assert!(engine.is_warmed());
        let endpoint = engine.native_position();
        let mut produced = Vec::new();
        for step in 0..100 {
            if let Some(point) = engine.sample(time + step * 1_000_000) {
                produced.push(point);
            }
        }
        assert_eq!(produced.last().copied(), Some(endpoint));
        assert_eq!(engine.sample(time + 1_000_000_000), None);
        assert!(!engine.has_pending());
    }

    #[test]
    fn never_extrapolates_past_newest_sample() {
        let mut engine = Engine::new(Point::ZERO);
        let trace = steady_trace(4, Point::new(10.0, 0.0), 8_000_000, Point::ZERO);
        let time = feed(&mut engine, 0, &trace);
        let newest = engine.native_position();
        let mut time = time;
        loop {
            time += 1_000_000;
            match engine.sample(time) {
                Some(sample) => assert!(sample.x <= newest.x + f64::EPSILON),
                None => break,
            }
        }
        assert_eq!(engine.sample(time), None);
    }

    #[test]
    fn slow_motion_still_moves() {
        let mut engine = Engine::new(Point::ZERO);
        let trace = steady_trace(20, Point::new(1.0, 0.0), 8_000_000, Point::ZERO);
        let time = feed(&mut engine, 0, &trace);
        let mut previous = engine.sample(time).unwrap();
        let start = previous.x;
        let mut advanced = 0.0;
        for step in 1..200 {
            match engine.sample(time + step * 1_000_000) {
                Some(point) => {
                    assert!(point.x >= previous.x - f64::EPSILON);
                    advanced = point.x - start;
                    previous = point;
                }
                None => break,
            }
        }
        assert!(advanced > 0.5, "slow movement must not be dropped");
        assert_eq!(previous, engine.native_position());
    }

    #[test]
    fn path_length_matches_native_polyline() {
        let mut engine = Engine::new(Point::ZERO);
        let interval = 8_000_000_u64;
        let step = Point::new(7.0, 3.0);
        let mut time = 0_u64;
        let mut previous = Point::ZERO;
        let mut emitted = Vec::new();
        let mut last_warm: Option<Point> = None;
        for index in 1..=8 {
            let target = step.scale(index as f64);
            let start = time;
            time += interval;
            engine.push_native(time, target.sub(previous)).unwrap();
            previous = target;
            let mut tick = start + 1_000_000;
            while tick <= time {
                if let Some(point) = engine.sample(tick)
                    && engine.is_warmed()
                {
                    emitted.push(point);
                }
                tick += 1_000_000;
            }
            if engine.is_warmed() && last_warm.is_none() {
                last_warm = Some(previous);
            }
        }
        let mut tick = time;
        loop {
            tick += 1_000_000;
            match engine.sample(tick) {
                Some(point) => emitted.push(point),
                None => break,
            }
        }
        let start = last_warm.expect("warmed during the trace");
        let mut native_length = 0.0;
        let mut point = start;
        for index in 3..=8 {
            let next = step.scale(index as f64);
            native_length += next.distance(point);
            point = next;
        }
        let mut emitted_length = 0.0;
        let mut previous = start;
        for point in emitted {
            emitted_length += point.distance(previous);
            previous = point;
        }
        assert!((emitted_length - native_length).abs() < 0.01);
        assert_eq!(previous, engine.native_position());
    }

    #[test]
    fn lag_scales_with_measured_interval_and_is_clamped() {
        for (interval, expected) in [
            (1_500_000_u64, MIN_LAG_NS),
            (8_000_000, 12_000_000),
            (20_000_000, 30_000_000),
            (40_000_000, MAX_LAG_NS),
        ] {
            let mut engine = Engine::new(Point::ZERO);
            let trace = steady_trace(4, Point::new(2.0, 0.0), interval, Point::ZERO);
            feed(&mut engine, 0, &trace);
            assert_eq!(engine.lag_ns(), Some(expected), "interval {interval}");
        }
    }

    #[test]
    fn no_catch_up_burst_after_late_wake() {
        let mut engine = Engine::new(Point::ZERO);
        let trace = steady_trace(6, Point::new(5.0, 0.0), 8_000_000, Point::ZERO);
        let time = feed(&mut engine, 0, &trace);
        let first = engine.sample(time + 1_000_000).unwrap();
        let second = engine.sample(time + 250_000_000);
        assert_eq!(second, Some(engine.native_position()));
        assert_ne!(first, second.unwrap_or(first));
        assert_eq!(engine.sample(time + 251_000_000), None);
    }

    #[test]
    fn coincident_and_backward_timestamps_stay_finite() {
        let mut engine = Engine::new(Point::ZERO);
        engine
            .push_native(10_000_000, Point::new(3.0, 0.0))
            .unwrap();
        engine
            .push_native(10_000_000, Point::new(3.0, 0.0))
            .unwrap();
        engine
            .push_native(5_000_000, Point::new(-3.0, 0.0))
            .unwrap();
        let point = engine.sample(10_000_000).expect("finite target");
        assert!(point.is_finite());
        assert!(engine.native_position().is_finite());
    }

    #[test]
    fn negative_and_origin_coordinates_are_preserved() {
        let mut engine = Engine::new(Point::new(-1920.0, -200.0));
        let trace = vec![
            (8_000_000, Point::new(-1900.0, -200.0)),
            (8_000_000, Point::new(-1850.0, -180.0)),
            (8_000_000, Point::new(-1840.0, -170.0)),
        ];
        feed(&mut engine, 0, &trace);
        assert_eq!(engine.native_position(), Point::new(-1840.0, -170.0));
        let grid = PixelGrid::new(Point::new(-1920.0, -200.0), Point::new(3840.0, 1200.0));
        assert_eq!(grid.quantize(Point::new(-1919.6, -199.6)), (-1920, -200));
        assert_eq!(grid.quantize(Point::new(-1839.7, -169.4)), (-1840, -169));
        assert_eq!(grid.quantize(Point::new(9000.0, 9000.0)), (1919, 999));
    }

    #[test]
    fn discard_drops_pending_target() {
        let mut engine = Engine::new(Point::ZERO);
        let trace = steady_trace(5, Point::new(4.0, 0.0), 8_000_000, Point::ZERO);
        let time = feed(&mut engine, 0, &trace);
        assert!(engine.has_pending());
        engine.discard_and_reanchor(time, Point::new(500.0, 500.0));
        assert!(!engine.has_pending());
        assert_eq!(engine.sample(time), None);
        assert_eq!(engine.warmup_left(), WARMUP_MOVES);
        assert_eq!(engine.native_position(), Point::new(500.0, 500.0));
    }

    #[test]
    fn reset_forgets_learned_intervals() {
        let mut engine = Engine::new(Point::ZERO);
        let trace = steady_trace(5, Point::new(4.0, 0.0), 8_000_000, Point::ZERO);
        let time = feed(&mut engine, 0, &trace);
        assert_eq!(engine.lag_ns(), Some(12_000_000));
        engine.reset_history(time, Point::ZERO);
        assert_eq!(engine.lag_ns(), None);
        assert_eq!(engine.warmup_left(), WARMUP_MOVES);
    }

    #[test]
    fn non_finite_input_is_rejected() {
        let mut engine = Engine::new(Point::ZERO);
        assert_eq!(
            engine.push_native(1, Point::new(f64::NAN, 0.0)),
            Err(Fault::NonFinite)
        );
        assert_eq!(
            engine.push_absolute(1, Point::new(0.0, f64::INFINITY)),
            Err(Fault::NonFinite)
        );
    }

    #[test]
    fn absolute_and_relative_mapping_produce_same_path() {
        let trace = steady_trace(
            6,
            Point::new(6.0, -2.0),
            8_000_000,
            Point::new(300.0, 400.0),
        );
        let mut relative = Engine::new(Point::new(300.0, 400.0));
        feed(&mut relative, 0, &trace);
        let mut absolute = Engine::new(Point::new(300.0, 400.0));
        let mut time = 0;
        for (dt, point) in &trace {
            time += *dt;
            absolute.push_absolute(time, *point).unwrap();
        }
        assert_eq!(relative.native_position(), absolute.native_position());
        assert_eq!(relative.lag_ns(), absolute.lag_ns());
    }
}
