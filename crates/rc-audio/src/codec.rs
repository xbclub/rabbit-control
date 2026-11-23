//! 纯 Rust IMA-ADPCM 音频编解码与 PCM 增益/静音处理。
//!
//! 每个编码帧都是**完全自包含**的：帧头为每声道 4 字节解码状态
//! (`[predictor: i16 LE][step_index: u8][reserved: u8]`)，随后紧跟打包的
//! 4-bit ADPCM 半字节（nibble）。即使网络丢弃任意音频帧，后续帧仍可立即
//! 无损同步解码，不会产生跨帧状态漂移。
//!
//! 20ms 48kHz 立体声 PCM16（1920 个 `i16` 采样 = 3840 字节）编码后为：
//! 8 字节双声道帧头 + 960 字节 nibble 载荷 = **968 字节**（约 387 kbps，~4:1 压缩）。

#![deny(unsafe_code)]
#![deny(missing_docs)]

/// 默认采样率（48 kHz）。
pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;

/// 默认声道数（双声道立体声）。
pub const DEFAULT_CHANNELS: u8 = 2;

/// 默认单帧时长（20 ms）。
pub const FRAME_DURATION_MS: u32 = 20;

/// 48 kHz 下单声道 20 ms 的采样点数（`48_000 * 20 / 1000 = 960`）。
pub const SAMPLES_PER_CHANNEL_20MS: usize = 960;

/// 每声道状态帧头字节数：`[predictor: i16 LE][step_index: u8][reserved: u8]`。
const CHANNEL_HEADER_BYTES: usize = 4;

/// `reserved` 字节标志位：仅用于单声道奇数采样长度时标记最后一个字节的高 4 位为填充。
const FLAG_MONO_ODD_TAIL: u8 = 0x01;

/// IMA-ADPCM 步长索引调整表（对应 4-bit code `0..=15`）。
const INDEX_TABLE: [i8; 16] = [-1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8];

/// IMA-ADPCM 89 级量化步长表（索引 `0..=88`）。
const STEP_TABLE: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449,
    494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272,
    2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493,
    10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];

/// 单声道 IMA-ADPCM 状态机。
#[derive(Debug, Clone, Copy)]
struct AdpcmState {
    predictor: i32,
    step_index: u8,
}

impl AdpcmState {
    fn new(predictor: i16, step_index: u8) -> Self {
        Self {
            predictor: i32::from(predictor),
            step_index: step_index.min(88),
        }
    }

    fn write_header(self, reserved: u8, out: &mut Vec<u8>) {
        let pred = self
            .predictor
            .clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
        out.extend_from_slice(&pred.to_le_bytes());
        out.push(self.step_index);
        out.push(reserved);
    }

    fn read_header(bytes: &[u8]) -> Result<(Self, u8), String> {
        if bytes.len() < CHANNEL_HEADER_BYTES {
            return Err("truncated IMA-ADPCM channel header".to_string());
        }
        let predictor = i16::from_le_bytes([bytes[0], bytes[1]]);
        let step_index = bytes[2];
        let reserved = bytes[3];
        if step_index > 88 {
            return Err(format!(
                "corrupt IMA-ADPCM header: step_index {step_index} out of range 0..=88"
            ));
        }
        Ok((Self::new(predictor, step_index), reserved))
    }

    #[inline]
    fn encode_nibble(&mut self, sample: i16) -> u8 {
        let step = STEP_TABLE[usize::from(self.step_index)];
        let mut diff = i32::from(sample) - self.predictor;
        let mut code: u8 = 0;
        if diff < 0 {
            code = 8;
            diff = -diff;
        }

        let mut vpdiff = step >> 3;
        if diff >= step {
            code |= 4;
            diff -= step;
            vpdiff += step;
        }
        let step2 = step >> 1;
        if diff >= step2 {
            code |= 2;
            diff -= step2;
            vpdiff += step2;
        }
        let step4 = step >> 2;
        if diff >= step4 {
            code |= 1;
            vpdiff += step4;
        }

        if (code & 8) != 0 {
            self.predictor -= vpdiff;
        } else {
            self.predictor += vpdiff;
        }
        self.predictor = self
            .predictor
            .clamp(i32::from(i16::MIN), i32::from(i16::MAX));

        let next_idx = i32::from(self.step_index) + i32::from(INDEX_TABLE[usize::from(code)]);
        self.step_index = next_idx.clamp(0, 88) as u8;
        code
    }

    #[inline]
    fn decode_nibble(&mut self, nibble: u8) -> i16 {
        let code = nibble & 0x0F;
        let step = STEP_TABLE[usize::from(self.step_index)];

        let mut vpdiff = step >> 3;
        if (code & 4) != 0 {
            vpdiff += step;
        }
        if (code & 2) != 0 {
            vpdiff += step >> 1;
        }
        if (code & 1) != 0 {
            vpdiff += step >> 2;
        }

        if (code & 8) != 0 {
            self.predictor -= vpdiff;
        } else {
            self.predictor += vpdiff;
        }
        self.predictor = self
            .predictor
            .clamp(i32::from(i16::MIN), i32::from(i16::MAX));

        let next_idx = i32::from(self.step_index) + i32::from(INDEX_TABLE[usize::from(code)]);
        self.step_index = next_idx.clamp(0, 88) as u8;
        self.predictor as i16
    }
}

/// 根据帧首两个采样的差分幅度估算合理的初始 `step_index`，
/// 消除自包含帧边界处从 `step_index = 0` 爬坡导致的瞬态失真；
/// 静音帧（差分 0）始终返回 `0` 以保持零底噪。
fn estimate_initial_step_index(pcm: &[i16], channels: usize, ch: usize) -> u8 {
    if pcm.len() < ch + channels + 1 {
        return 0;
    }
    let delta = (i32::from(pcm[ch + channels]) - i32::from(pcm[ch])).abs();
    if delta == 0 {
        return 0;
    }
    STEP_TABLE
        .iter()
        .position(|&step| step >= delta)
        .unwrap_or(STEP_TABLE.len() - 1) as u8
}

/// 将 16-bit 线性 PCM（单声道或交错立体声）编码为自包含的 IMA-ADPCM 数据包。
///
/// 帧布局：
/// - 单声道（`channels == 1`）：4 字节声道头 + 每字节 2 个采样（低 4 位在前，高 4 位在后）。
///   若采样数为奇数，帧头 `reserved` 置为 `0x01`，末字节高 4 位填 0。
/// - 立体声（`channels == 2`）：8 字节双声道头（L 后 R）+ 每字节 1 个立体声帧
///   （低 4 位为左声道，高 4 位为右声道）。20ms 48kHz 立体声（1920 采样）恰好编码为 968 字节。
/// - 不支持的声道数返回空 `Vec`。
#[must_use]
pub fn encode_ima_adpcm(pcm: &[i16], channels: u8) -> Vec<u8> {
    match channels {
        1 => {
            if pcm.is_empty() {
                let mut out = Vec::with_capacity(CHANNEL_HEADER_BYTES);
                AdpcmState::new(0, 0).write_header(0, &mut out);
                return out;
            }
            let odd_tail = if pcm.len() % 2 == 1 {
                FLAG_MONO_ODD_TAIL
            } else {
                0
            };
            let mut state = AdpcmState::new(pcm[0], estimate_initial_step_index(pcm, 1, 0));
            let nibble_bytes = pcm.len().div_ceil(2);
            let mut out = Vec::with_capacity(CHANNEL_HEADER_BYTES + nibble_bytes);
            state.write_header(odd_tail, &mut out);

            let mut chunks = pcm.chunks_exact(2);
            for pair in chunks.by_ref() {
                let lo = state.encode_nibble(pair[0]);
                let hi = state.encode_nibble(pair[1]);
                out.push((lo & 0x0F) | ((hi & 0x0F) << 4));
            }
            if let Some(&last) = chunks.remainder().first() {
                let lo = state.encode_nibble(last);
                out.push(lo & 0x0F);
            }
            out
        }
        2 => {
            let frames = pcm.len() / 2;
            let header_bytes = CHANNEL_HEADER_BYTES * 2;
            if frames == 0 {
                let mut out = Vec::with_capacity(header_bytes);
                AdpcmState::new(0, 0).write_header(0, &mut out);
                AdpcmState::new(0, 0).write_header(0, &mut out);
                return out;
            }
            let mut left = AdpcmState::new(pcm[0], estimate_initial_step_index(pcm, 2, 0));
            let mut right = AdpcmState::new(pcm[1], estimate_initial_step_index(pcm, 2, 1));
            let mut out = Vec::with_capacity(header_bytes + frames);
            left.write_header(0, &mut out);
            right.write_header(0, &mut out);

            for pair in pcm.chunks_exact(2) {
                let lo = left.encode_nibble(pair[0]);
                let hi = right.encode_nibble(pair[1]);
                out.push((lo & 0x0F) | ((hi & 0x0F) << 4));
            }
            out
        }
        _ => Vec::new(),
    }
}

/// 将自包含 IMA-ADPCM 数据包解码回 16-bit 线性 PCM（单声道或交错立体声）。
///
/// 会严格校验声道数、帧头长度、`step_index` 范围（`0..=88`）与保留标志位，
/// 截断或损坏输入均返回可读的 `Err(String)` 而绝不 panic。
pub fn decode_ima_adpcm(data: &[u8], channels: u8) -> Result<Vec<i16>, String> {
    match channels {
        1 => {
            if data.len() < CHANNEL_HEADER_BYTES {
                return Err(format!(
                    "truncated mono IMA-ADPCM packet: got {} bytes, need at least {CHANNEL_HEADER_BYTES}",
                    data.len()
                ));
            }
            let (mut state, reserved) = AdpcmState::read_header(&data[..CHANNEL_HEADER_BYTES])?;
            if reserved != 0 && reserved != FLAG_MONO_ODD_TAIL {
                return Err(format!(
                    "corrupt mono IMA-ADPCM header: unexpected reserved byte 0x{reserved:02x}"
                ));
            }
            let payload = &data[CHANNEL_HEADER_BYTES..];
            if reserved == FLAG_MONO_ODD_TAIL && payload.is_empty() {
                return Err(
                    "corrupt mono IMA-ADPCM packet: odd-tail flag set with empty payload"
                        .to_string(),
                );
            }
            let total_samples = payload.len() * 2 - usize::from(reserved == FLAG_MONO_ODD_TAIL);
            let mut out = Vec::with_capacity(total_samples);
            for (idx, &byte) in payload.iter().enumerate() {
                out.push(state.decode_nibble(byte & 0x0F));
                let is_last_padded = reserved == FLAG_MONO_ODD_TAIL && idx + 1 == payload.len();
                if !is_last_padded {
                    out.push(state.decode_nibble((byte >> 4) & 0x0F));
                }
            }
            Ok(out)
        }
        2 => {
            let header_bytes = CHANNEL_HEADER_BYTES * 2;
            if data.len() < header_bytes {
                return Err(format!(
                    "truncated stereo IMA-ADPCM packet: got {} bytes, need at least {header_bytes}",
                    data.len()
                ));
            }
            let (mut left, res_l) = AdpcmState::read_header(&data[..CHANNEL_HEADER_BYTES])?;
            let (mut right, res_r) =
                AdpcmState::read_header(&data[CHANNEL_HEADER_BYTES..header_bytes])?;
            if res_l != 0 || res_r != 0 {
                return Err(format!(
                    "corrupt stereo IMA-ADPCM header: unexpected reserved bytes (0x{res_l:02x}, 0x{res_r:02x})"
                ));
            }
            let payload = &data[header_bytes..];
            let mut out = Vec::with_capacity(payload.len() * 2);
            for &byte in payload {
                out.push(left.decode_nibble(byte & 0x0F));
                out.push(right.decode_nibble((byte >> 4) & 0x0F));
            }
            Ok(out)
        }
        _ => Err(format!(
            "unsupported IMA-ADPCM channel count: {channels} (expected 1 or 2)"
        )),
    }
}

/// 原地对 PCM 缓冲区应用静音或线性增益（带 `i16` 饱和限幅）。
///
/// - 当 `muted == true`、`gain <= 0.0` 或 `gain` 非有限数（`NaN`/`inf`）时，直接清零；
/// - 当 `gain == 1.0` 且未静音时，走无拷贝快速路径；
/// - 其余情况按 `gain` 线性缩放并四舍五入钳位到 `[i16::MIN, i16::MAX]`。
pub fn apply_mute_or_gain(pcm: &mut [i16], muted: bool, gain: f32) {
    if muted || !gain.is_finite() || gain <= 0.0 {
        pcm.fill(0);
        return;
    }
    if (gain - 1.0).abs() <= f32::EPSILON {
        return;
    }
    for sample in pcm.iter_mut() {
        let scaled = (f32::from(*sample) * gain)
            .round()
            .clamp(f32::from(i16::MIN), f32::from(i16::MAX));
        *sample = scaled as i16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    fn sine_wave_pcm(
        sample_rate: u32,
        channels: u8,
        frames: usize,
        freq_l: f64,
        freq_r: f64,
        amplitude: f64,
        phase_offset_frames: usize,
    ) -> Vec<i16> {
        let mut out = Vec::with_capacity(frames * usize::from(channels));
        for i in 0..frames {
            let t = (phase_offset_frames + i) as f64 / f64::from(sample_rate);
            let l = ((TAU * freq_l * t).sin() * amplitude).round() as i16;
            out.push(l);
            if channels == 2 {
                let r = ((TAU * freq_r * t).sin() * amplitude).round() as i16;
                out.push(r);
            }
        }
        out
    }

    fn compute_snr_db(reference: &[i16], decoded: &[i16]) -> f64 {
        assert_eq!(reference.len(), decoded.len());
        let mut signal_power = 0.0_f64;
        let mut noise_power = 0.0_f64;
        for (&r, &d) in reference.iter().zip(decoded.iter()) {
            let rf = f64::from(r);
            let err = rf - f64::from(d);
            signal_power += rf * rf;
            noise_power += err * err;
        }
        assert!(signal_power > 0.0, "reference signal must be non-zero");
        if noise_power == 0.0 {
            return f64::INFINITY;
        }
        10.0 * (signal_power / noise_power).log10()
    }

    #[test]
    fn stereo_20ms_packet_size_and_snr() {
        let pcm = sine_wave_pcm(
            DEFAULT_SAMPLE_RATE,
            DEFAULT_CHANNELS,
            SAMPLES_PER_CHANNEL_20MS,
            440.0,
            660.0,
            20_000.0,
            0,
        );
        assert_eq!(pcm.len(), 1920);

        let encoded = encode_ima_adpcm(&pcm, 2);
        // 8-byte header + 960 bytes nibbles = 968 bytes
        assert_eq!(encoded.len(), 968);

        let decoded = decode_ima_adpcm(&encoded, 2).expect("stereo decode should succeed");
        assert_eq!(decoded.len(), pcm.len());

        let snr = compute_snr_db(&pcm, &decoded);
        assert!(
            snr > 25.0,
            "stereo 440Hz/660Hz sine SNR expected > 25 dB, got {snr:.2} dB"
        );
    }

    #[test]
    fn mono_even_and_odd_round_trip_snr() {
        let pcm = sine_wave_pcm(
            DEFAULT_SAMPLE_RATE,
            1,
            SAMPLES_PER_CHANNEL_20MS,
            440.0,
            440.0,
            22_000.0,
            0,
        );
        let encoded = encode_ima_adpcm(&pcm, 1);
        assert_eq!(encoded.len(), 4 + 480);

        let decoded = decode_ima_adpcm(&encoded, 1).expect("mono decode should succeed");
        assert_eq!(decoded.len(), pcm.len());
        let snr = compute_snr_db(&pcm, &decoded);
        assert!(
            snr > 25.0,
            "mono 440Hz sine SNR expected > 25 dB, got {snr:.2} dB"
        );

        // Odd sample count preserves exact length via odd-tail flag.
        let odd_pcm = &pcm[..333];
        let odd_encoded = encode_ima_adpcm(odd_pcm, 1);
        let odd_decoded = decode_ima_adpcm(&odd_encoded, 1).expect("odd mono decode should work");
        assert_eq!(odd_decoded.len(), 333);
        let odd_snr = compute_snr_db(odd_pcm, &odd_decoded);
        assert!(odd_snr > 25.0, "odd mono SNR {odd_snr:.2} dB");
    }

    #[test]
    fn silence_stays_at_zero() {
        let stereo_silence = vec![0i16; SAMPLES_PER_CHANNEL_20MS * 2];
        let enc_s = encode_ima_adpcm(&stereo_silence, 2);
        let dec_s = decode_ima_adpcm(&enc_s, 2).expect("stereo silence decode");
        assert_eq!(dec_s.len(), stereo_silence.len());
        assert!(dec_s.iter().all(|&s| s.abs() <= 1));

        let mono_silence = vec![0i16; SAMPLES_PER_CHANNEL_20MS];
        let enc_m = encode_ima_adpcm(&mono_silence, 1);
        let dec_m = decode_ima_adpcm(&enc_m, 1).expect("mono silence decode");
        assert_eq!(dec_m.len(), mono_silence.len());
        assert!(dec_m.iter().all(|&s| s.abs() <= 1));
    }

    #[test]
    fn self_contained_packets_survive_dropped_frames() {
        // Encode 5 consecutive 20ms stereo frames, drop frames 1 and 3,
        // and verify frame 2 and frame 4 decode independently with SNR > 25 dB.
        let mut packets = Vec::new();
        let mut originals = Vec::new();
        for idx in 0..5 {
            let pcm = sine_wave_pcm(
                DEFAULT_SAMPLE_RATE,
                2,
                SAMPLES_PER_CHANNEL_20MS,
                440.0,
                440.0,
                18_000.0,
                idx * SAMPLES_PER_CHANNEL_20MS,
            );
            packets.push(encode_ima_adpcm(&pcm, 2));
            originals.push(pcm);
        }

        // Only decode frames 0, 2, 4 (frames 1 and 3 dropped).
        for idx in [0, 2, 4] {
            let decoded = decode_ima_adpcm(&packets[idx], 2).expect("independent frame decode");
            let snr = compute_snr_db(&originals[idx], &decoded);
            assert!(
                snr > 25.0,
                "frame {idx} after dropped neighbor expected SNR > 25 dB, got {snr:.2} dB"
            );
        }
    }

    #[test]
    fn rejects_truncated_and_corrupt_inputs() {
        assert!(decode_ima_adpcm(&[], 1).is_err());
        assert!(decode_ima_adpcm(&[0, 1, 2], 1).is_err());
        assert!(decode_ima_adpcm(&[0; 7], 2).is_err());

        // Invalid channel count.
        assert!(decode_ima_adpcm(&[0; 16], 0).is_err());
        assert!(decode_ima_adpcm(&[0; 16], 3).is_err());
        assert!(encode_ima_adpcm(&[1, 2, 3], 3).is_empty());

        // step_index out of bounds (> 88).
        assert!(decode_ima_adpcm(&[0, 0, 89, 0, 0x12], 1).is_err());
        assert!(decode_ima_adpcm(&[0, 0, 10, 0, 0, 0, 200, 0, 0x12], 2).is_err());

        // Corrupt reserved byte.
        assert!(decode_ima_adpcm(&[0, 0, 0, 0x02, 0x12], 1).is_err());
        assert!(decode_ima_adpcm(&[0, 0, 0, 0x01], 1).is_err());
        assert!(decode_ima_adpcm(&[0, 0, 0, 0x01, 0, 0, 0, 0, 0x12], 2).is_err());
    }

    #[test]
    fn apply_mute_or_gain_behaves_correctly() {
        let mut buf = vec![1000i16, -2000, 20_000, -25_000];
        apply_mute_or_gain(&mut buf, false, 1.0);
        assert_eq!(buf, vec![1000, -2000, 20_000, -25_000]);

        apply_mute_or_gain(&mut buf, false, 0.5);
        assert_eq!(buf, vec![500, -1000, 10_000, -12_500]);

        // Clamping on high gain.
        apply_mute_or_gain(&mut buf, false, 5.0);
        assert_eq!(buf, vec![2500, -5000, i16::MAX, i16::MIN]);

        // Mute zeroes buffer regardless of gain.
        apply_mute_or_gain(&mut buf, true, 1.0);
        assert_eq!(buf, vec![0, 0, 0, 0]);

        // NaN / negative gain also silences cleanly.
        let mut buf2 = vec![1234, -5678];
        apply_mute_or_gain(&mut buf2, false, f32::NAN);
        assert_eq!(buf2, vec![0, 0]);
    }
}
