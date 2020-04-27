use ffmpeg::Rational;
use std::time::Duration;

pub(crate) fn to_duration(time_ref: i64, time_base: Rational) -> Duration {
    Duration::from_secs_f64((time_ref as f64 / time_base.1 as f64) * time_base.0 as f64)
}
