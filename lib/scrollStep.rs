/// Wheel steps count `wheel` each (fractions for hi-res wheels), smooth deltas scale by `smooth`.
pub fn scroll_step(dy: f64, wheel_unit: bool, wheel: f64, smooth: f64) -> f64 {
    dy * if wheel_unit { wheel } else { smooth }
}
