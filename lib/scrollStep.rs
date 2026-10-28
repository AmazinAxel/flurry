/// Wheel steps count `wheel` each, smooth deltas scale by `smooth` (lib/scrollStep.ts).
pub fn scroll_step(dy: f64, wheel_unit: bool, wheel: f64, smooth: f64) -> f64 {
    if wheel_unit { dy.signum() * wheel } else { dy * smooth }
}
