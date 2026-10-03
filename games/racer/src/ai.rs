//! Computer drivers: follow the racing line with a personal lane bias, brake
//! for corners from the curvature ahead, avoid cars in front, and adjust
//! their pace slightly to keep the race close (rubber banding).

use vmath::{FloatExt, Vec2};

use crate::car::{Car, Controls, TOP_SPEED};
use crate::track::{HALF_WIDTH, SPACING, Track};

/// Personality of a computer driver.
#[derive(Clone, Copy, Debug)]
pub struct Driver {
    /// Pace multiplier (about 0.9 .. 1.0).
    pub skill: f32,
    /// Preferred offset from the racing line (m).
    pub lane: f32,
    /// Cornering grip the driver trusts (m/s^2).
    pub grip: f32,
    /// Current avoidance offset (m), eased.
    pub dodge: f32,
}

impl Driver {
    pub fn new(skill: f32, lane: f32) -> Driver {
        Driver { skill, lane, grip: 15.0 * skill, dodge: 0.0 }
    }

    /// Controls for `me` given the other cars and the pace factor from
    /// rubber banding.
    pub fn drive(&mut self, me: usize, cars: &[Car], track: &Track, pace: f32, dt: f32) -> Controls {
        let car = &cars[me];
        let speed = car.speed();
        // Look ahead further at speed.
        let look = 9.0 + speed.max(0.0) * 0.55;
        let s_target = car.s + look;
        // Avoid a car ahead in our lane.
        let mut want_dodge = 0.0;
        for (j, o) in cars.iter().enumerate() {
            if j == me {
                continue;
            }
            let mut ds = o.s - car.s;
            if ds < -track.length * 0.5 {
                ds += track.length;
            }
            if ds > 0.0 && ds < 16.0 {
                let dl = o.lateral - car.lateral;
                if dl.abs() < 2.6 {
                    want_dodge = if dl > 0.0 { -3.2 } else { 3.2 };
                }
            }
        }
        self.dodge += (want_dodge - self.dodge) * (1.0 - (-dt * 2.0).exp());
        let limit = HALF_WIDTH - 2.0;
        let lateral = (track.line_at(s_target) + self.lane + self.dodge).clamp(-limit, limit);
        let target = track.point(s_target, lateral);
        let to = Vec2::new(target.x - car.pos.x, target.z - car.pos.z);
        let f = car.forward2();
        let l = car.left2();
        let angle = to.dot(l).atan2(to.dot(f));
        // Pure pursuit of the look-ahead point, plus a correction for the
        // sideways error where the car is now (keeps it off the grass at
        // speed), damped by the yaw rate.
        let here = (track.line_at(car.s) + self.lane + self.dodge).clamp(-limit, limit);
        let cross = ((here - car.lateral) * 0.06).clamp(-0.4, 0.4);
        let steer = (angle * 2.6 + cross - car.yaw_rate * 0.12).clamp(-1.0, 1.0);

        // Target speed: what the corners ahead allow given the braking
        // distance to each of them.
        let top = TOP_SPEED * self.skill * pace;
        let mut allowed = top;
        let decel = 18.0;
        let steps = (90.0 / SPACING) as isize;
        for k in 1..steps {
            let smp = track.at(car.index as isize + k);
            let d = k as f32 * SPACING;
            let v_corner = (self.grip * pace / smp.curv.abs().max(1e-4)).sqrt();
            let v = (v_corner * v_corner + 2.0 * decel * d).sqrt();
            allowed = allowed.min(v);
        }
        let mut c = Controls { steer, ..Controls::default() };
        if speed < allowed - 1.0 {
            c.throttle = 1.0;
        } else if speed > allowed + 2.5 {
            c.brake = ((speed - allowed) / 8.0).clamp(0.3, 1.0);
        } else {
            c.throttle = 0.35;
        }
        // Recover when stuck or going backwards.
        if speed < 2.0 && angle.abs() > 1.2 {
            c.throttle = 0.6;
            c.steer = angle.signum();
        }
        c
    }
}
