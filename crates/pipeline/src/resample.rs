//! Streaming conversion of mono audio to the 16 kHz the speech pipeline uses.

use std::f64::consts::PI;

/// Sample rate of everything after the resampler: VAD, utterances and the
/// audio sent to speech-to-text.
pub const PIPELINE_RATE: u32 = 16_000;

/// Input samples on each side of the output position that feed one output
/// sample, scaled by the downsampling factor so the filter keeps its shape.
const HALF_TAPS_PER_OUTPUT_SAMPLE: f64 = 16.0;
/// Kernel table resolution: entries per input sample of offset.
const TABLE_STEPS: usize = 256;

/// Windowed-sinc resampler for a stream that arrives in arbitrary blocks.
///
/// Output sample `n` is the input signal at time `n / out_rate`, low-pass
/// filtered below the output's Nyquist frequency, so the output stays aligned
/// to session time. The first outputs only appear once `half_width` input
/// samples past them have arrived (1 ms at 48 kHz).
pub struct Resampler {
    in_rate: u64,
    out_rate: u64,
    /// Input samples from absolute index `buf_start` on.
    buf: Vec<f32>,
    buf_start: u64,
    /// Total input samples received.
    received: u64,
    /// Output samples produced so far.
    produced: u64,
    half_width: i64,
    /// Kernel value at offset `i / TABLE_STEPS` input samples, for `i` in `0..=half_width * TABLE_STEPS`.
    table: Vec<f32>,
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        assert!(in_rate > 0 && out_rate > 0, "sample rates must be positive");
        let ratio = out_rate as f64 / in_rate as f64;
        // Cut off a little below the lower Nyquist frequency.
        let cutoff = ratio.min(1.0) * 0.92;
        let half_width = (HALF_TAPS_PER_OUTPUT_SAMPLE / ratio.min(1.0)).ceil() as i64;
        let len = half_width as usize * TABLE_STEPS + 1;
        let table = (0..len)
            .map(|i| {
                let x = i as f64 / TABLE_STEPS as f64;
                let sinc = if x == 0.0 {
                    1.0
                } else {
                    (PI * cutoff * x).sin() / (PI * cutoff * x)
                };
                // Blackman window over [-half_width, half_width].
                let blackman = 0.42
                    + 0.5 * (PI * x / half_width as f64).cos()
                    + 0.08 * (2.0 * PI * x / half_width as f64).cos();
                (cutoff * sinc * blackman) as f32
            })
            .collect();
        Self {
            in_rate: in_rate as u64,
            out_rate: out_rate as u64,
            buf: Vec::new(),
            buf_start: 0,
            received: 0,
            produced: 0,
            half_width,
            table,
        }
    }

    /// Adds input and returns every output sample that can now be computed.
    pub fn push(&mut self, input: &[f32]) -> Vec<f32> {
        if self.in_rate == self.out_rate {
            self.received += input.len() as u64;
            self.produced += input.len() as u64;
            return input.to_vec();
        }
        self.buf.extend_from_slice(input);
        self.received += input.len() as u64;
        self.drain(self.received as i64)
    }

    /// Ends the stream: treats everything after the last input as silence and
    /// returns the remaining outputs up to the end of the input.
    pub fn finish(&mut self) -> Vec<f32> {
        if self.in_rate == self.out_rate {
            return Vec::new();
        }
        // Outputs whose window runs past the end see zeros there.
        self.drain(self.received as i64 + self.half_width)
    }

    /// Produces outputs whose filter window ends before input index `available`.
    fn drain(&mut self, available: i64) -> Vec<f32> {
        let mut out = Vec::new();
        loop {
            let center = (self.produced * self.in_rate / self.out_rate) as i64;
            if center + self.half_width >= available {
                break;
            }
            out.push(self.sample_at(self.produced));
            self.produced += 1;
        }
        // Keep only the input the next output still needs.
        let next_center = (self.produced * self.in_rate / self.out_rate) as i64;
        let keep_from = (next_center - self.half_width + 1).max(0) as u64;
        if keep_from > self.buf_start {
            let drop = ((keep_from - self.buf_start) as usize).min(self.buf.len());
            self.buf.drain(..drop);
            self.buf_start += drop as u64;
        }
        out
    }

    /// Output sample `n`, computed from the buffered input (zero outside it).
    fn sample_at(&self, n: u64) -> f32 {
        let num = n * self.in_rate;
        let center = (num / self.out_rate) as i64;
        let frac = (num % self.out_rate) as f64 / self.out_rate as f64;
        let mut acc = 0f32;
        for k in (center - self.half_width + 1)..=(center + self.half_width) {
            let idx = k - self.buf_start as i64;
            if idx < 0 || idx as usize >= self.buf.len() {
                continue;
            }
            let offset = ((k - center) as f64 - frac).abs();
            let pos = offset * TABLE_STEPS as f64;
            let i = pos as usize;
            if i + 1 >= self.table.len() {
                continue;
            }
            let t = (pos - i as f64) as f32;
            let weight = self.table[i] * (1.0 - t) + self.table[i + 1] * t;
            acc += self.buf[idx as usize] * weight;
        }
        acc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, freq: f64, seconds: f64) -> Vec<f32> {
        (0..(rate as f64 * seconds) as usize)
            .map(|i| (2.0 * PI * freq * i as f64 / rate as f64).sin() as f32 * 0.5)
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    fn run(r: &mut Resampler, input: &[f32], block: usize) -> Vec<f32> {
        let mut out = Vec::new();
        for chunk in input.chunks(block) {
            out.extend(r.push(chunk));
        }
        out.extend(r.finish());
        out
    }

    #[test]
    fn output_length_matches_duration() {
        for (rate, block) in [(48_000, 480), (44_100, 441), (48_000, 7), (16_000, 100)] {
            let input = vec![0.1f32; rate as usize * 2];
            let out = run(&mut Resampler::new(rate, PIPELINE_RATE), &input, block);
            assert_eq!(out.len(), 32_000, "rate {rate} block {block}");
        }
    }

    #[test]
    fn keeps_speech_band_and_removes_aliases() {
        // 1 kHz passes through unchanged in level.
        let out = run(
            &mut Resampler::new(48_000, PIPELINE_RATE),
            &sine(48_000, 1000.0, 1.0),
            512,
        );
        let level = rms(&out[1000..15_000]);
        assert!((level - 0.3536).abs() < 0.01, "1 kHz level {level}");
        // 12 kHz is above the 8 kHz output Nyquist and must be filtered out,
        // not folded back as a 4 kHz alias.
        let out = run(
            &mut Resampler::new(48_000, PIPELINE_RATE),
            &sine(48_000, 12_000.0, 1.0),
            512,
        );
        let level = rms(&out[1000..15_000]);
        assert!(level < 0.01, "12 kHz leaked at {level}");
    }

    #[test]
    fn stays_aligned_in_time() {
        // An impulse at 0.5 s comes out at 0.5 s.
        let mut input = vec![0f32; 48_000];
        input[24_000] = 1.0;
        let out = run(&mut Resampler::new(48_000, PIPELINE_RATE), &input, 333);
        let peak = out
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .unwrap()
            .0;
        assert_eq!(peak, 8_000);
    }

    #[test]
    fn block_size_does_not_change_output() {
        let input = sine(44_100, 440.0, 0.5);
        let a = run(&mut Resampler::new(44_100, PIPELINE_RATE), &input, 64);
        let b = run(&mut Resampler::new(44_100, PIPELINE_RATE), &input, 4410);
        assert_eq!(a.len(), b.len());
        assert!(a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1e-6));
    }
}
