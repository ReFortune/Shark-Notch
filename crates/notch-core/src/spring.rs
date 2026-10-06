//! Damped-spring animation with an exact (closed-form) solver.
//!
//! * **Exact for any `dt`**: a dropped frame advances the spring by the real elapsed time and lands
//!   exactly where a sequence of tiny steps would have. No integrator error, no instability.
//! * **Interruptible**: [`Spring::set_target`] keeps position *and* velocity, so retargeting
//!   mid-flight bends the motion instead of restarting it.
//! * Parametrised by angular frequency `omega` (rad/s) and damping ratio `zeta`
//!   (`< 1` overshoots, `1` is critical, `> 1` creeps in without overshoot).

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpringParams {
    /// Undamped natural angular frequency in rad/s. Higher is snappier.
    pub omega: f32,
    /// Damping ratio.
    pub zeta: f32,
}

impl SpringParams {
    pub const fn new(omega: f32, zeta: f32) -> Self {
        Self { omega, zeta }
    }

    /// Build from the classic mass/stiffness/damping-coefficient triple.
    pub fn from_physical(stiffness: f32, damping: f32, mass: f32) -> Self {
        let omega = (stiffness / mass).sqrt();
        let zeta = damping / (2.0 * (stiffness * mass).sqrt());
        Self { omega, zeta }
    }

    /// Scale the speed (not the damping ratio): `2.0` plays twice as fast with the same shape.
    pub fn faster(self, k: f32) -> Self {
        Self {
            omega: self.omega * k,
            zeta: self.zeta,
        }
    }

    /// Peak overshoot as a fraction of the travel distance for a step response (0 if not underdamped).
    pub fn overshoot(&self) -> f32 {
        if self.zeta >= 1.0 {
            0.0
        } else {
            (-(std::f32::consts::PI * self.zeta) / (1.0 - self.zeta * self.zeta).sqrt()).exp()
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Spring {
    x: f64,
    v: f64,
    target: f64,
    params: SpringParams,
    /// Position tolerance below which (together with `rest_vel`) the spring counts as settled.
    rest_pos: f64,
    rest_vel: f64,
}

impl Spring {
    pub fn new(value: f32, params: SpringParams) -> Self {
        Self {
            x: value as f64,
            v: 0.0,
            target: value as f64,
            params,
            rest_pos: 0.01,
            rest_vel: 0.05,
        }
    }

    /// Tune the settle tolerances (e.g. tighter for alpha than for pixels).
    pub fn with_rest(mut self, pos: f32, vel: f32) -> Self {
        self.rest_pos = pos as f64;
        self.rest_vel = vel as f64;
        self
    }

    pub fn value(&self) -> f32 {
        self.x as f32
    }

    pub fn velocity(&self) -> f32 {
        self.v as f32
    }

    pub fn target(&self) -> f32 {
        self.target as f32
    }

    pub fn params(&self) -> SpringParams {
        self.params
    }

    pub fn set_params(&mut self, p: SpringParams) {
        self.params = p;
    }

    /// Retarget without touching position or velocity.
    pub fn set_target(&mut self, t: f32) {
        self.target = t as f64;
    }

    /// Jump to `v` with zero velocity (used for "reduce motion" and first placement).
    pub fn snap(&mut self, v: f32) {
        self.x = v as f64;
        self.v = 0.0;
        self.target = v as f64;
    }

    /// Give the spring an extra kick (e.g. a small "pop" when something arrives).
    pub fn impulse(&mut self, dv: f32) {
        self.v += dv as f64;
    }

    pub fn is_settled(&self) -> bool {
        (self.x - self.target).abs() <= self.rest_pos && self.v.abs() <= self.rest_vel
    }

    /// Advance by `dt` seconds. Returns the new value. Settled springs snap exactly to the target so
    /// callers can stop their frame loop.
    pub fn step(&mut self, dt: f32) -> f32 {
        let dt = (dt as f64).clamp(0.0, 1.0);
        if dt > 0.0 {
            let (x, v) = advance(
                self.x - self.target,
                self.v,
                self.params.omega as f64,
                self.params.zeta as f64,
                dt,
            );
            self.x = self.target + x;
            self.v = v;
        }
        if self.is_settled() {
            self.x = self.target;
            self.v = 0.0;
        }
        self.x as f32
    }
}

/// Closed-form solution of `x'' + 2ζω x' + ω² x = 0` from `(d0, v0)` after `t` seconds.
fn advance(d0: f64, v0: f64, omega: f64, zeta: f64, t: f64) -> (f64, f64) {
    if omega <= 1e-9 {
        return (d0 + v0 * t, v0);
    }
    if (zeta - 1.0).abs() < 1e-4 {
        // Critically damped.
        let e = (-omega * t).exp();
        let x = e * (d0 + (v0 + omega * d0) * t);
        let v = e * (v0 - omega * (v0 + omega * d0) * t);
        (x, v)
    } else if zeta < 1.0 {
        // Underdamped.
        let wd = omega * (1.0 - zeta * zeta).sqrt();
        let e = (-zeta * omega * t).exp();
        let (s, c) = (wd * t).sin_cos();
        let b = (v0 + zeta * omega * d0) / wd;
        let x = e * (d0 * c + b * s);
        let v = e * (v0 * c - (zeta * omega * v0 + omega * omega * d0) / wd * s);
        (x, v)
    } else {
        // Overdamped.
        let q = omega * (zeta * zeta - 1.0).sqrt();
        let r1 = -zeta * omega + q;
        let r2 = -zeta * omega - q;
        let c1 = (v0 - r2 * d0) / (r1 - r2);
        let c2 = d0 - c1;
        let (e1, e2) = ((r1 * t).exp(), (r2 * t).exp());
        (c1 * e1 + c2 * e2, c1 * r1 * e1 + c2 * r2 * e2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Brute-force RK4 reference integrator.
    fn rk4(mut x: f64, mut v: f64, target: f64, p: SpringParams, t: f64, n: usize) -> (f64, f64) {
        let (w, z) = (p.omega as f64, p.zeta as f64);
        let h = t / n as f64;
        let acc = |x: f64, v: f64| -2.0 * z * w * v - w * w * (x - target);
        for _ in 0..n {
            let (k1x, k1v) = (v, acc(x, v));
            let (k2x, k2v) = (v + 0.5 * h * k1v, acc(x + 0.5 * h * k1x, v + 0.5 * h * k1v));
            let (k3x, k3v) = (v + 0.5 * h * k2v, acc(x + 0.5 * h * k2x, v + 0.5 * h * k2v));
            let (k4x, k4v) = (v + h * k3v, acc(x + h * k3x, v + h * k3v));
            x += h / 6.0 * (k1x + 2.0 * k2x + 2.0 * k3x + k4x);
            v += h / 6.0 * (k1v + 2.0 * k2v + 2.0 * k3v + k4v);
        }
        (x, v)
    }

    fn assert_close(a: f64, b: f64, tol: f64, what: &str) {
        assert!((a - b).abs() <= tol, "{what}: {a} vs {b}");
    }

    #[test]
    fn closed_form_matches_rk4_for_all_damping_regimes() {
        for zeta in [0.2, 0.5, 0.8, 1.0, 1.0001, 1.5, 4.0] {
            let p = SpringParams::new(22.0, zeta);
            for t in [0.01, 0.05, 0.2, 0.7] {
                let mut s = Spring::new(10.0, p);
                s.set_target(100.0);
                s.impulse(300.0);
                let (rx, rv) = rk4(10.0, 300.0, 100.0, p, t, 20_000);
                // Use the raw solver so the settle-snap does not interfere.
                let (dx, dv) = advance(10.0 - 100.0, 300.0, p.omega as f64, p.zeta as f64, t);
                assert_close(
                    100.0 + dx,
                    rx,
                    1e-5 * rx.abs().max(1.0),
                    &format!("x zeta={zeta} t={t}"),
                );
                assert_close(
                    dv,
                    rv,
                    1e-4 * rv.abs().max(1.0),
                    &format!("v zeta={zeta} t={t}"),
                );
                let _ = &mut s;
            }
        }
    }

    #[test]
    fn stepping_is_dt_invariant() {
        // One big step equals many small ones (exactness is what makes dropped frames safe).
        let p = SpringParams::new(25.0, 0.7);
        let mut a = Spring::new(0.0, p);
        a.set_target(200.0);
        let mut b = a;
        a.step(0.5);
        for _ in 0..500 {
            b.step(0.001);
        }
        assert_close(a.value() as f64, b.value() as f64, 1e-3, "position");
        assert_close(a.velocity() as f64, b.velocity() as f64, 1e-2, "velocity");
    }

    #[test]
    fn underdamped_overshoot_matches_theory() {
        let p = SpringParams::new(20.0, 0.6);
        let mut s = Spring::new(0.0, p);
        s.set_target(1.0);
        let mut peak = 0.0f32;
        for _ in 0..4000 {
            peak = peak.max(s.step(0.0005));
        }
        assert_close(peak as f64, 1.0 + p.overshoot() as f64, 2e-3, "peak");
    }

    #[test]
    fn critical_and_overdamped_never_overshoot() {
        for zeta in [1.0, 1.2, 3.0] {
            let mut s = Spring::new(0.0, SpringParams::new(30.0, zeta));
            s.set_target(50.0);
            for _ in 0..600 {
                assert!(s.step(1.0 / 120.0) <= 50.0 + 1e-3, "zeta {zeta} overshot");
            }
        }
    }

    #[test]
    fn settles_and_snaps_exactly() {
        let mut s = Spring::new(0.0, SpringParams::new(24.0, 0.8));
        s.set_target(77.0);
        let mut steps = 0;
        while !s.is_settled() {
            s.step(1.0 / 60.0);
            steps += 1;
            assert!(steps < 600, "never settled");
        }
        assert_eq!(s.step(1.0 / 60.0), 77.0);
        assert_eq!(s.velocity(), 0.0);
    }

    #[test]
    fn retarget_preserves_position_and_velocity() {
        let mut s = Spring::new(0.0, SpringParams::new(22.0, 0.75));
        s.set_target(100.0);
        for _ in 0..8 {
            s.step(1.0 / 60.0);
        }
        let (x, v) = (s.value(), s.velocity());
        assert!(v > 0.0 && x > 0.0 && x < 100.0);
        s.set_target(20.0); // reverse direction mid-flight
        assert_eq!(s.value(), x, "position continuous");
        assert_eq!(s.velocity(), v, "velocity continuous");
        // First step after retargeting must be continuous: it keeps moving the old way briefly
        // (momentum) instead of teleporting.
        let next = s.step(1.0 / 240.0);
        assert!((next - x).abs() < 3.0, "jump {}", next - x);
        // And it ends at the new target.
        for _ in 0..600 {
            s.step(1.0 / 60.0);
        }
        assert_eq!(s.value(), 20.0);
    }

    #[test]
    fn retarget_midflight_is_smoother_than_restarting() {
        // Velocity continuity: compare the velocity just after retargeting with a fresh spring.
        let p = SpringParams::new(22.0, 0.75);
        let mut live = Spring::new(0.0, p);
        live.set_target(100.0);
        for _ in 0..6 {
            live.step(1.0 / 60.0);
        }
        let v_before = live.velocity();
        live.set_target(0.0);
        live.step(1.0 / 1000.0);
        let mut fresh = Spring::new(live.value(), p);
        fresh.set_target(0.0);
        fresh.step(1.0 / 1000.0);
        assert!(
            (live.velocity() - v_before).abs() < 0.2 * v_before.abs(),
            "live keeps momentum"
        );
        assert!(fresh.velocity().abs() < live.velocity().abs() || v_before.abs() < 1.0);
    }

    #[test]
    fn zero_and_huge_dt_are_safe() {
        let mut s = Spring::new(5.0, SpringParams::new(20.0, 0.5));
        s.set_target(9.0);
        assert_eq!(s.step(0.0), 5.0);
        assert_eq!(s.step(-1.0), 5.0, "negative dt ignored");
        s.step(1e6); // clamped
        assert!(s.value().is_finite());
        assert!((s.value() - 9.0).abs() < 1e-3);
    }

    #[test]
    fn physical_parameters_convert() {
        let p = SpringParams::from_physical(400.0, 31.0, 1.0);
        assert_close(p.omega as f64, 20.0, 1e-4, "omega");
        assert_close(p.zeta as f64, 0.775, 1e-4, "zeta");
        assert_close(p.faster(2.0).omega as f64, 40.0, 1e-4, "faster");
        assert_eq!(p.faster(2.0).zeta, p.zeta);
    }
}
