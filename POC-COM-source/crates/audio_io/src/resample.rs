//! Linear-interpolation resampling between a device's native sample rate
//! and the modem's internal rate. Deliberately kept off the real-time
//! audio callback thread -- it runs once, on the calling thread, after a
//! transmission is prepared or a reception finishes.

pub fn resample(input: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate || input.is_empty() {
        return input.to_vec();
    }
    let ratio = from_rate as f32 / to_rate as f32;
    let out_len = ((input.len() as f32) / ratio).max(1.0) as usize;
    (0..out_len)
        .map(|i| {
            let src_pos = i as f32 * ratio;
            let idx = src_pos.floor() as usize;
            let frac = src_pos - idx as f32;
            let a = *input.get(idx).unwrap_or_else(|| input.last().unwrap());
            let b = *input.get(idx + 1).unwrap_or_else(|| input.last().unwrap());
            a + (b - a) * frac
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_rate_is_identity() {
        let input = vec![1.0, 2.0, 3.0, 4.0];
        assert_eq!(resample(&input, 48_000, 48_000), input);
    }

    #[test]
    fn downsampling_shrinks_length_proportionally() {
        let input: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.01).sin()).collect();
        let out = resample(&input, 48_000, 8_000);
        let expected = input.len() / 6;
        assert!((out.len() as isize - expected as isize).abs() <= 1);
    }

    #[test]
    fn upsampling_grows_length_proportionally() {
        let input: Vec<f32> = (0..800).map(|i| (i as f32 * 0.05).sin()).collect();
        let out = resample(&input, 8_000, 48_000);
        let expected = input.len() * 6;
        assert!((out.len() as isize - expected as isize).abs() <= 6);
    }
}
