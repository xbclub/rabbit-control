//! S4 性能 HUD 真数据差分聚合（T21 API 契约 §2）。
//!
//! 全部纯函数、时间与观测值全部注入（铁律 4）：前端不做本地时钟计算，
//! 只格式化渲染本模块产出的 [`HudSnapshot`]。

#![deny(unsafe_code)]

/// 观测原始快照（会话线程从 `ClientSession` 访问器取；全部注入数据）。
#[derive(Clone, Debug, PartialEq)]
pub struct HudSample {
    /// 采样时刻（微秒，会话钟）。
    pub at_us: u64,
    /// 累计上屏帧数（`controller.displayed` 语义）。
    pub displayed: u64,
    /// 累计接收字节（`NetStats.bytes_recv`）。
    pub bytes_recv: u64,
    /// 探测平滑 RTT（微秒；无样本时 `None`）。
    pub rtt_us: Option<u32>,
    /// 丢包率（`NetStats.loss_rate`，范围 `0.0..=1.0`）。
    pub loss_rate: f32,
    /// 当前帧时间 p50（毫秒；偏移换算后的 e2e 最近窗口，无样本 `None`）。
    pub e2e_p50_ms: Option<f64>,
    /// Welcome 裁定宽度（像素）。
    pub width: u32,
    /// Welcome 裁定高度（像素）。
    pub height: u32,
}

/// S4 HUD 快照（`serde::Serialize` 仅用于 Tauri IPC 边界）。
#[derive(serde::Serialize, Clone, Debug, PartialEq)]
pub struct HudSnapshot {
    /// `"00:00:11"`（会话钟换算；71.6min 饱和刷新由 T17 守卫，UI 无需处理）。
    pub elapsed: String,
    /// 传输摘要，例如 `"UDP LAN"` 或 `"TCP Relay"`。
    pub transport: &'static str,
    /// 区间平均帧率（帧/秒）。
    pub fps: f64,
    /// 区间平均接收码率（kbps）。
    pub bitrate_kbps: f64,
    /// 平滑 RTT（毫秒），无样本时 `None`。
    pub rtt_ms: Option<f64>,
    /// 帧时间 p50（毫秒），无样本时 `None`。
    pub frame_ms: Option<f64>,
    /// 丢包百分比（`0.0..=100.0`）。
    pub loss_pct: f64,
    /// 分辨率标签，如 `"1920x1080"`。
    pub resolution: String,
}

impl HudSnapshot {
    /// 由相邻两次观测差分计算（纯函数，时间注入，单测钉死）：
    /// - `fps = Δdisplayed / Δt`
    /// - `bitrate_kbps = Δbytes_recv × 8 / Δt / 1000`
    /// - `Δt == 0` 或首样本（无 `prev`）时 `fps` / `bitrate_kbps` 均为 `0.0`（诚实显示，不沿旧值）。
    pub fn derive(prev: Option<&HudSample>, cur: &HudSample) -> Self {
        Self::derive_with_transport(prev, cur, "UDP LAN")
    }

    /// 同 [`Self::derive`]，但由调用方显式指定传输层标签（如 `"UDP LAN"` 或 `"TCP Relay"`）。
    pub fn derive_with_transport(
        prev: Option<&HudSample>,
        cur: &HudSample,
        transport: &'static str,
    ) -> Self {
        let (fps, bitrate_kbps) = match prev {
            Some(p) if cur.at_us > p.at_us => {
                let dt_s = (cur.at_us - p.at_us) as f64 / 1_000_000.0;
                let d_frames = cur.displayed.saturating_sub(p.displayed) as f64;
                let d_bytes = cur.bytes_recv.saturating_sub(p.bytes_recv) as f64;
                let fps = d_frames / dt_s;
                let bitrate_kbps = (d_bytes * 8.0) / dt_s / 1000.0;
                (
                    if fps.is_finite() { fps } else { 0.0 },
                    if bitrate_kbps.is_finite() {
                        bitrate_kbps
                    } else {
                        0.0
                    },
                )
            }
            _ => (0.0, 0.0),
        };

        let rtt_ms = cur.rtt_us.map(|us| f64::from(us) / 1000.0);
        let frame_ms = cur.e2e_p50_ms.filter(|v| v.is_finite() && *v >= 0.0);
        let loss_raw = f64::from(cur.loss_rate) * 100.0;
        let loss_pct = if loss_raw.is_finite() {
            loss_raw.clamp(0.0, 100.0)
        } else {
            0.0
        };

        Self {
            elapsed: format_elapsed_hms(cur.at_us),
            transport,
            fps,
            bitrate_kbps,
            rtt_ms,
            frame_ms,
            loss_pct,
            resolution: format!("{}x{}", cur.width, cur.height),
        }
    }
}

/// 将会话微秒钟格式化为 `HH:MM:SS`。
fn format_elapsed_hms(at_us: u64) -> String {
    let total_secs = at_us / 1_000_000;
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    format!("{hours:02}:{mins:02}:{secs:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_sample() -> HudSample {
        HudSample {
            at_us: 0,
            displayed: 0,
            bytes_recv: 0,
            rtt_us: Some(4_500),
            loss_rate: 0.0125,
            e2e_p50_ms: Some(12.3),
            width: 1920,
            height: 1080,
        }
    }

    #[test]
    fn hud_derive_computes_deltas() {
        let s0 = base_sample();
        // 首样本（无 prev）：fps / bitrate 必须为 0
        let first = HudSnapshot::derive(None, &s0);
        assert_eq!(first.fps, 0.0);
        assert_eq!(first.bitrate_kbps, 0.0);
        assert_eq!(first.transport, "UDP LAN");
        assert_eq!(first.resolution, "1920x1080");
        assert_eq!(first.rtt_ms, Some(4.5));
        assert_eq!(first.frame_ms, Some(12.3));
        assert!((first.loss_pct - 1.25).abs() < 1e-6);

        // 1 秒后：Δdisplayed=60 -> fps=60.0；Δbytes=125000 -> bitrate_kbps=1000.0
        let s1 = HudSample {
            at_us: 1_000_000,
            displayed: 60,
            bytes_recv: 125_000,
            ..s0.clone()
        };
        let snap = HudSnapshot::derive(Some(&s0), &s1);
        assert!((snap.fps - 60.0).abs() < 1e-9);
        assert!((snap.bitrate_kbps - 1000.0).abs() < 1e-9);

        // Δt == 0 不产生 NaN，归零
        let same_t = HudSnapshot::derive(Some(&s1), &s1);
        assert_eq!(same_t.fps, 0.0);
        assert_eq!(same_t.bitrate_kbps, 0.0);
        assert!(!same_t.fps.is_nan());
        assert!(!same_t.bitrate_kbps.is_nan());
    }

    #[test]
    fn hud_elapsed_formats_hms() {
        let mut s = base_sample();
        s.at_us = 0;
        assert_eq!(HudSnapshot::derive(None, &s).elapsed, "00:00:00");

        s.at_us = 11_999_999;
        assert_eq!(HudSnapshot::derive(None, &s).elapsed, "00:00:11");

        s.at_us = 3_661_000_000; // 1h 1m 1s
        assert_eq!(HudSnapshot::derive(None, &s).elapsed, "01:01:01");

        // 71 分钟（4260s）不越界
        s.at_us = 4_260_000_000;
        assert_eq!(HudSnapshot::derive(None, &s).elapsed, "01:11:00");
    }

    #[test]
    fn hud_zero_division_is_safe() {
        let bad = HudSample {
            at_us: 500_000,
            displayed: 10,
            bytes_recv: 1000,
            rtt_us: None,
            loss_rate: f32::NAN,
            e2e_p50_ms: Some(f64::NAN),
            width: 1280,
            height: 720,
        };
        let prev_later = HudSample {
            at_us: 1_000_000,
            ..bad.clone()
        };
        // 逆序时间戳（cur.at_us < prev.at_us）与 NaN 输入均不得产出 NaN
        let snap = HudSnapshot::derive(Some(&prev_later), &bad);
        assert_eq!(snap.fps, 0.0);
        assert_eq!(snap.bitrate_kbps, 0.0);
        assert_eq!(snap.rtt_ms, None);
        assert_eq!(snap.frame_ms, None);
        assert_eq!(snap.loss_pct, 0.0);

        let inf_sample = HudSample {
            loss_rate: f32::INFINITY,
            e2e_p50_ms: Some(f64::INFINITY),
            ..bad
        };
        let snap_inf = HudSnapshot::derive(None, &inf_sample);
        assert_eq!(snap_inf.loss_pct, 0.0);
        assert_eq!(snap_inf.frame_ms, None);
        assert!(!snap_inf.fps.is_nan());
        assert!(!snap_inf.bitrate_kbps.is_nan());
    }

    #[test]
    fn hud_derive_with_transport_preserves_custom_label() {
        let s0 = base_sample();
        let s1 = HudSample {
            at_us: 500_000,
            displayed: 30,
            bytes_recv: 62_500,
            ..s0.clone()
        };
        let snap = HudSnapshot::derive_with_transport(Some(&s0), &s1, "TCP Relay");
        assert_eq!(snap.transport, "TCP Relay");
        assert!((snap.fps - 60.0).abs() < 1e-9);
        assert!((snap.bitrate_kbps - 1000.0).abs() < 1e-9);
        assert_eq!(snap.rtt_ms, Some(4.5));
    }
}
