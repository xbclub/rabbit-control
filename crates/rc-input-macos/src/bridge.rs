//! 把 [`rc_input`] 的事件桥接到本 crate 的 [`Injector`]（T10 适配层）。
//!
//! 职责只有**翻译**：远控端发来的归一化事件（0..65535 坐标、Set 1
//! 扫描码）还原成本 crate 的全局坐标 / 逻辑键事件，再交给既有的
//! 注入链路（[`Injector`] → [`plan`] → `EventPoster`）。权限预检、
//! 坐标钳制、滚动取整等防御都在既有链路里，本层不重复实现。
//!
//! # 坐标映射（I4 反归一化）
//!
//! 构造时从 [`Injector::screen_bounds`] 取**全部在线显示器的联合边界**
//! （原点可为负——左侧副屏），把 `0..=65535` 线性铺进该边界；
//! [`rc_input::InputInjector::set_screen_size`] 覆盖宽高（保留原点）。
//!
//! # 已知限制（见任务卡 / concerns）
//!
//! - `X1`/`X2` 侧键：CGEvent 路径只有左/右/中三键（[`PointerButtonKind`]），
//!   显式返回错误而不是静默丢弃。错误自 2026-11-21 起在批内隔离
//!   （同 `rc_input::InputChannel::pump_receiver` 语义）：单条翻译 /
//!   投递失败只记首错，同批剩余事件继续处理，兄弟事件（含松键）
//!   不再随批蒸发；
//! - 按钮事件不带坐标（线格式如此）：用批内最近一次移动的位置；
//!   尚无移动时退到屏幕中心（TODO：接入 `CGEventSourceMousePosition`
//!   读真实光标）。

use crate::error::InputError;
use crate::event::{InputEvent as MacEvent, PointerButtonKind};
use crate::injector::Injector;
use crate::plan::{EventPoster, ScreenBounds};
use crate::scancode::key_from_set1;
use rc_input::{InputEvent, InputInjector, MouseButton};
use rc_types::{Error, Result};

/// [`rc_input::InputInjector`] 的 macOS 实现：包装本 crate 的 [`Injector`]。
///
/// 泛型 [`EventPoster`] 沿用 [`Injector`] 的注入缝——测试用假投递器
/// 即可验证全链路翻译，真投递（`CgPoster`）需要辅助功能授权。
/// trait 超特征要求 `Send`，因此实现要求 `P: Send`（`CgPoster` 的
/// `Send` 见 `ffi` 模块的 SAFETY 说明）。
pub struct InputBridge<P: EventPoster + Send = crate::ffi::CgPoster> {
    injector: Injector<P>,
    /// 联合边界的原点（多显示器负象限，反归一化的平移分量）。
    origin: (f64, f64),
    /// 映射目标宽高（点），构造时取自屏幕边界，可被 `set_screen_size` 覆盖。
    size: (f64, f64),
    /// 批内最近一次指针位置（按钮事件的坐标兜底）。
    last_pos: Option<(f64, f64)>,
}

impl InputBridge<crate::ffi::CgPoster> {
    /// 生产构造：走 [`Injector::new`]（辅助功能预检 → CGEventSource →
    /// 屏幕联合边界）。未授权立即返回 [`InputError::NotTrusted`]。
    pub fn new() -> Result<Self, InputError> {
        let injector = Injector::new()?;
        Ok(Self::from_injector(injector))
    }

    /// 生产构造（主显示器模式）：走 [`Injector::for_main_display`]，使归一化坐标
    /// `0..=65535` 精确映射到主显示器边界（与 `SckConfig { display: 0 }` 对齐）。
    pub fn for_main_display() -> Result<Self, InputError> {
        let injector = Injector::for_main_display()?;
        Ok(Self::from_injector(injector))
    }
}

impl<P: EventPoster + Send> InputBridge<P> {
    /// 由现成注入器组装（测试 / 上层已持有注入器时用）。
    pub fn from_injector(injector: Injector<P>) -> Self {
        let b = injector.screen_bounds();
        Self {
            injector,
            origin: (b.x, b.y),
            size: (b.width, b.height),
            last_pos: None,
        }
    }

    /// 归一化坐标 `0..=65535` → 联合边界内的全局坐标（点）。
    ///
    /// `0 ↦ origin`、`65535 ↦ origin + size - 1`（与 [`rc_input`] 的
    /// `normalize_coord/denormalize_coord` 端点约定一致）。
    fn map_point(&self, x: u16, y: u16) -> (f64, f64) {
        let fx = self.origin.0 + (f64::from(x) / 65535.0) * (self.size.0 - 1.0).max(0.0);
        let fy = self.origin.1 + (f64::from(y) / 65535.0) * (self.size.1 - 1.0).max(0.0);
        (fx, fy)
    }

    /// 当前映射目标（诊断 / 测试）。
    pub fn screen_bounds(&self) -> ScreenBounds {
        ScreenBounds::new(self.origin.0, self.origin.1, self.size.0, self.size.1)
    }

    /// 单条事件翻译（更新 `last_pos`）。`Sync` 翻译为 `None`（无注入语义）。
    fn convert(&mut self, ev: &InputEvent) -> std::result::Result<Option<MacEvent>, Error> {
        match ev {
            InputEvent::MouseMove { x, y } => {
                let (px, py) = self.map_point(*x, *y);
                self.last_pos = Some((px, py));
                Ok(Some(MacEvent::PointerMove { x: px, y: py }))
            }
            InputEvent::MouseButton { button, pressed } => {
                let kind = match button {
                    MouseButton::Left => PointerButtonKind::Left,
                    MouseButton::Right => PointerButtonKind::Right,
                    MouseButton::Middle => PointerButtonKind::Middle,
                    MouseButton::X1 | MouseButton::X2 => {
                        return Err(Error::InvalidConfig(format!(
                            "macOS CGEvent 注入路径不支持侧键 {button:?}（PointerButtonKind 只有左/右/中）"
                        )));
                    }
                };
                // 线格式的按钮事件不带坐标：用批内最近移动位置，
                // 没有则退到屏幕中心（确定性兜底，TODO 读真实光标）。
                let (x, y) = self.last_pos.unwrap_or_else(|| {
                    (
                        self.origin.0 + self.size.0 / 2.0,
                        self.origin.1 + self.size.1 / 2.0,
                    )
                });
                Ok(Some(MacEvent::PointerButton {
                    button: kind,
                    pressed: *pressed,
                    x,
                    y,
                }))
            }
            InputEvent::MouseWheel { delta, horizontal } => {
                // rc-input：delta>0 垂直=向上、水平=向右；
                // 本 crate：dy>0 向上、dx>0 向左 → 水平翻符号。
                // 量纲直传：rc-input 的 MouseWheel.delta 规范单位是**行**
                // （见 InputEvent::MouseWheel 文档）——macOS CG 行单位直传，
                // Windows 端 WHEEL_DELTA=120/格在 rc-input-win 除以 120
                // 归一为行，两端汇到本层都是行，注入侧按 LINE 单位投递。
                let (dx, dy) = if *horizontal {
                    (-f64::from(*delta), 0.0)
                } else {
                    (0.0, f64::from(*delta))
                };
                Ok(Some(MacEvent::PointerScroll { dx, dy }))
            }
            InputEvent::Key {
                scancode, pressed, ..
            } => {
                // modifiers 字段无需处理（I8）：修饰键本身以独立 Key 事件
                // 到达并逐个注入，接收端不推断状态。
                let key = key_from_set1(*scancode).ok_or_else(|| Error::Protocol {
                    offset: 0,
                    msg: format!("unknown Set-1 scancode 0x{scancode:04X} (macOS 键表未覆盖)"),
                })?;
                Ok(Some(MacEvent::Key {
                    key,
                    pressed: *pressed,
                }))
            }
            InputEvent::Sync => Ok(None),
        }
    }
}

impl<P: EventPoster + Send> InputInjector for InputBridge<P> {
    /// 批内错误隔离（与 `rc_input::InputChannel::pump_receiver` 同语义）：
    /// 单条 convert / 注入失败**不提前返回**——失败只记首个错误，同批
    /// 剩余事件（可能含松键，被吞 = 用户端永久卡键）继续翻译注入，
    /// 批处理完后返回首个错误；失败事件本身不可恢复（翻译已拒 / 投递
    /// 已试过），就地丢弃、只报这一次错，不缓存重试。
    fn inject(&mut self, events: &[InputEvent]) -> Result<()> {
        let mut first_err: Option<Error> = None;
        for ev in events {
            // convert 失败：记首错后继续本批剩余事件（Sync → None 无注入语义）。
            let mac = match self.convert(ev) {
                Ok(mac) => mac,
                Err(e) => {
                    first_err.get_or_insert(e);
                    continue;
                }
            };
            // 注入失败：同样只记首错，不阻断同批兄弟事件。
            if let Some(mac) = mac {
                if let Err(e) = self.injector.inject(mac) {
                    first_err
                        .get_or_insert(Error::InvalidConfig(format!("macOS inject failed: {e}")));
                }
            }
        }
        first_err.map_or(Ok(()), Err)
    }

    fn set_screen_size(&mut self, w: u32, h: u32) {
        let wf = f64::from(w);
        let hf = f64::from(h);
        self.size = (wf, hf);
        self.injector
            .set_screen_bounds(ScreenBounds::new(self.origin.0, self.origin.1, wf, hf));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{PlannedEvent, PlannedMouseKind};
    use std::sync::{Arc, Mutex};

    /// 假投递器（复刻 injector.rs 测试的做法：只记录，不碰系统）。
    ///
    /// 与 injector.rs 不同，这里用 `Arc<Mutex>` 而不是 `Rc<RefCell>`：
    /// `rc_input::InputInjector` 的超特征要求 `Send`，桥接的假投递器
    /// 必须同样可跨线程。
    #[derive(Clone, Default)]
    struct FakePoster {
        posted: Arc<Mutex<Vec<PlannedEvent>>>,
    }

    impl EventPoster for FakePoster {
        fn post(&mut self, planned: &PlannedEvent) -> std::result::Result<(), InputError> {
            self.posted.lock().expect("posted").push(*planned);
            Ok(())
        }
    }

    fn bridge_with(
        screen: ScreenBounds,
    ) -> (InputBridge<FakePoster>, Arc<Mutex<Vec<PlannedEvent>>>) {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let poster = FakePoster {
            posted: Arc::clone(&recorded),
        };
        let injector = Injector::from_parts(poster, screen, true).expect("trusted parts");
        (InputBridge::from_injector(injector), recorded)
    }

    /// 生产形态满足 trait 契约的 `Send` 超特征（CgPoster 的 Send 见 ffi）。
    #[test]
    fn production_bridge_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<InputBridge>();
    }

    /// I4 反归一化：0/65535 与联合边界端点精确对齐（含负原点副屏）。
    #[test]
    fn bridge_maps_normalized_move_into_bounds() {
        let (mut bridge, recorded) = bridge_with(ScreenBounds::new(-1920.0, 0.0, 4480.0, 1080.0));
        bridge
            .inject(&[
                InputEvent::MouseMove { x: 0, y: 0 },
                InputEvent::MouseMove {
                    x: u16::MAX,
                    y: u16::MAX,
                },
                InputEvent::MouseMove { x: 32768, y: 32768 },
            ])
            .expect("inject moves");

        let posted = recorded.lock().expect("posted");
        assert_eq!(posted.len(), 3);
        // 端点对齐：0 ↦ 左上（负原点），65535 ↦ 右下角
        assert_eq!(
            posted[0],
            PlannedEvent::Mouse {
                kind: PlannedMouseKind::Moved,
                button: PointerButtonKind::Left,
                x: -1920.0,
                y: 0.0,
            }
        );
        let PlannedEvent::Mouse { x, y, .. } = posted[1] else {
            panic!("must be Mouse");
        };
        assert!((x - 2559.0).abs() < 0.1, "x={x}");
        assert!((y - 1079.0).abs() < 0.1, "y={y}");
        // 中心落在联合边界中部
        let PlannedEvent::Mouse { x, y, .. } = posted[2] else {
            panic!("must be Mouse");
        };
        assert!(
            x > -1920.0 && x < 2560.0 && y > 0.0 && y < 1080.0,
            "({x},{y})"
        );
    }

    /// set_screen_size 覆盖宽高、保留原点：映射随新尺寸重算。
    #[test]
    fn bridge_set_screen_size_remaps() {
        let (mut bridge, recorded) = bridge_with(ScreenBounds::new(0.0, 0.0, 1920.0, 1080.0));
        bridge.set_screen_size(100, 200);
        assert_eq!(
            bridge.screen_bounds(),
            ScreenBounds::new(0.0, 0.0, 100.0, 200.0)
        );
        bridge
            .inject(&[InputEvent::MouseMove {
                x: u16::MAX,
                y: u16::MAX,
            }])
            .unwrap();
        let posted = recorded.lock().expect("posted");
        let PlannedEvent::Mouse { x, y, .. } = posted[0] else {
            panic!("must be Mouse");
        };
        assert!(
            (x - 99.0).abs() < 0.01 && (y - 199.0).abs() < 0.01,
            "({x},{y})"
        );
    }

    /// 放大分辨率（1512x982 -> 1800x1169）后，set_screen_size 必须同步更新
    /// Injector::screen_bounds，点击右下角附近 (1799, 1168) 不得被旧边界钳制。
    #[test]
    fn bridge_set_screen_size_updates_injector_bounds_without_clamping() {
        let (mut bridge, recorded) = bridge_with(ScreenBounds::new(0.0, 0.0, 1512.0, 982.0));
        bridge.set_screen_size(1800, 1169);
        assert_eq!(
            bridge.screen_bounds(),
            ScreenBounds::new(0.0, 0.0, 1800.0, 1169.0)
        );
        assert_eq!(
            bridge.injector.screen_bounds(),
            ScreenBounds::new(0.0, 0.0, 1800.0, 1169.0)
        );
        bridge
            .inject(&[
                InputEvent::MouseMove {
                    x: u16::MAX,
                    y: u16::MAX,
                },
                InputEvent::MouseButton {
                    button: MouseButton::Left,
                    pressed: true,
                },
            ])
            .expect("inject near expanded bottom-right");
        let posted = recorded.lock().expect("posted");
        assert_eq!(posted.len(), 2);
        let PlannedEvent::Mouse { x: mx, y: my, .. } = posted[0] else {
            panic!("must be MouseMove");
        };
        assert!(
            (mx - 1799.0).abs() < 0.01 && (my - 1168.0).abs() < 0.01,
            "放大分辨率后移动坐标不得被旧 1512x982 边界钳制，实际 ({mx},{my})"
        );
        let PlannedEvent::Mouse {
            kind,
            button,
            x: bx,
            y: by,
        } = posted[1]
        else {
            panic!("must be MouseButton");
        };
        assert_eq!(kind, PlannedMouseKind::Down);
        assert_eq!(button, PointerButtonKind::Left);
        assert!(
            (bx - 1799.0).abs() < 0.01 && (by - 1168.0).abs() < 0.01,
            "放大分辨率后点击坐标不得被旧 1512x982 边界钳制，实际 ({bx},{by})"
        );
    }

    /// 按钮：批内移动后的按钮用该坐标；未知侧键显式报错（不静默丢）。
    #[test]
    fn bridge_buttons_use_latest_position_and_reject_side_buttons() {
        let (mut bridge, recorded) = bridge_with(ScreenBounds::new(0.0, 0.0, 1000.0, 1000.0));
        bridge
            .inject(&[
                InputEvent::MouseMove { x: 32768, y: 32768 },
                InputEvent::MouseButton {
                    button: MouseButton::Right,
                    pressed: true,
                },
            ])
            .unwrap();
        let posted = recorded.lock().expect("posted");
        let PlannedEvent::Mouse { kind, button, x, y } = posted[1] else {
            panic!("must be Mouse");
        };
        assert_eq!(kind, PlannedMouseKind::Down);
        assert_eq!(button, PointerButtonKind::Right);
        // 与前一移动同坐标（约中心）
        let PlannedEvent::Mouse { x: mx, y: my, .. } = posted[0] else {
            panic!("must be Mouse");
        };
        assert_eq!((x, y), (mx, my));

        // X1 / X2：显式错误
        for button in [MouseButton::X1, MouseButton::X2] {
            let err = bridge
                .inject(&[InputEvent::MouseButton {
                    button,
                    pressed: true,
                }])
                .unwrap_err();
            assert!(err.to_string().contains("侧键"), "{err}");
        }
    }

    /// 没有任何移动时，按钮退到屏幕中心（确定性兜底路径）。
    #[test]
    fn bridge_button_without_prior_move_falls_back_to_center() {
        let (mut bridge, recorded) = bridge_with(ScreenBounds::new(100.0, 50.0, 1000.0, 500.0));
        bridge
            .inject(&[InputEvent::MouseButton {
                button: MouseButton::Left,
                pressed: false,
            }])
            .unwrap();
        let posted = recorded.lock().expect("posted");
        let PlannedEvent::Mouse { x, y, .. } = posted[0] else {
            panic!("must be Mouse");
        };
        assert_eq!((x, y), (600.0, 300.0));
    }

    /// 滚轮符号映射：垂直正=向上，水平正(向右) → macOS dx 取负（向左正）。
    #[test]
    fn bridge_converts_wheel_signs() {
        let (mut bridge, recorded) = bridge_with(ScreenBounds::new(0.0, 0.0, 100.0, 100.0));
        bridge
            .inject(&[
                InputEvent::MouseWheel {
                    delta: 3,
                    horizontal: false,
                },
                InputEvent::MouseWheel {
                    delta: 3,
                    horizontal: true,
                },
            ])
            .unwrap();
        let posted = recorded.lock().expect("posted");
        assert_eq!(posted[0], PlannedEvent::Scroll { dx: 0, dy: 3 });
        assert_eq!(posted[1], PlannedEvent::Scroll { dx: -3, dy: 0 });
    }

    /// 扫描码 → 逻辑键 → macOS 虚拟键码全链；未知扫描码显式报错。
    #[test]
    fn bridge_maps_scancode_to_virtual_key() {
        let (mut bridge, recorded) = bridge_with(ScreenBounds::new(0.0, 0.0, 100.0, 100.0));
        bridge
            .inject(&[
                InputEvent::Key {
                    scancode: 0x1E, // Set 1 A
                    pressed: true,
                    modifiers: 0x02, // 修饰键快照由独立事件注入，这里验证透传忽略
                },
                InputEvent::Key {
                    scancode: 0xE01D, // 右 Ctrl
                    pressed: false,
                    modifiers: 0,
                },
            ])
            .unwrap();
        let posted = recorded.lock().expect("posted");
        assert_eq!(
            posted[0],
            PlannedEvent::Key {
                keycode: 0x00, // kVK_ANSI_A
                pressed: true,
            }
        );
        assert_eq!(
            posted[1],
            PlannedEvent::Key {
                keycode: 0x3E, // kVK_RightControl
                pressed: false,
            }
        );

        let err = bridge
            .inject(&[InputEvent::Key {
                scancode: 0x7F,
                pressed: true,
                modifiers: 0,
            }])
            .unwrap_err();
        assert!(err.to_string().contains("scancode"), "{err}");
    }

    /// Sync 是心跳：无注入动作、不报错。
    #[test]
    fn bridge_sync_is_noop() {
        let (mut bridge, recorded) = bridge_with(ScreenBounds::new(0.0, 0.0, 100.0, 100.0));
        bridge
            .inject(&[InputEvent::Sync])
            .expect("sync must be a no-op");
        assert!(
            recorded.lock().expect("posted").is_empty(),
            "Sync 不得投递任何事件"
        );
    }

    /// 批内隔离（翻译失败）：批 [X1 按下, A 按下, A 松开] 中 X1 的
    /// convert 错误不得吞掉后面的松键——批尾返回含「侧键」的首错，
    /// 假投递器仍记录到两条 Key 事件且顺序保留。修复前 `?` 提前返回，
    /// 坏事件之后的同批事件全部蒸发（松键丢失 = 用户端永久卡键）。
    #[test]
    fn side_button_error_does_not_drop_sibling_key_events_in_batch() {
        let (mut bridge, recorded) = bridge_with(ScreenBounds::new(0.0, 0.0, 100.0, 100.0));
        let err = bridge
            .inject(&[
                InputEvent::MouseButton {
                    button: MouseButton::X1,
                    pressed: true,
                },
                InputEvent::Key {
                    scancode: 0x1E, // Set 1 A
                    pressed: true,
                    modifiers: 0,
                },
                InputEvent::Key {
                    scancode: 0x1E,
                    pressed: false,
                    modifiers: 0,
                },
            ])
            .expect_err("批内含侧键仍必须返回错误");
        assert!(err.to_string().contains("侧键"), "{err}");

        let posted = recorded.lock().expect("posted");
        assert_eq!(
            *posted,
            vec![
                PlannedEvent::Key {
                    keycode: 0x00, // kVK_ANSI_A
                    pressed: true,
                },
                PlannedEvent::Key {
                    keycode: 0x00, // kVK_ANSI_A
                    pressed: false,
                },
            ],
            "坏事件后面的按下/松开必须照常投递且顺序不变"
        );
    }

    /// 可编程失败的假投递器：前 `fail_first` 次投递返回错误，之后
    /// 成功；每次尝试（无论成败）都计数——验证单条投递失败不吞
    /// 同批兄弟事件。
    #[derive(Clone)]
    struct FlakyPoster {
        posted: Arc<Mutex<Vec<PlannedEvent>>>,
        calls: Arc<Mutex<usize>>,
        /// 前 fail_first 次投递必失败。
        fail_first: usize,
    }

    impl EventPoster for FlakyPoster {
        fn post(&mut self, planned: &PlannedEvent) -> std::result::Result<(), InputError> {
            let mut calls = self.calls.lock().expect("calls");
            *calls += 1;
            if *calls <= self.fail_first {
                return Err(InputError::EventCreateFailed("flaky poster"));
            }
            self.posted.lock().expect("posted").push(*planned);
            Ok(())
        }
    }

    /// 批内隔离（投递失败）：首条投递失败后，同批后续事件仍被尝试
    /// 投递，批尾返回首个错误（保留既有「macOS inject failed」包装）。
    #[test]
    fn inject_failure_does_not_drop_sibling_events_in_batch() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(Mutex::new(0usize));
        let poster = FlakyPoster {
            posted: Arc::clone(&recorded),
            calls: Arc::clone(&calls),
            fail_first: 1,
        };
        let injector =
            Injector::from_parts(poster, ScreenBounds::new(0.0, 0.0, 100.0, 100.0), true)
                .expect("trusted parts");
        let mut bridge = InputBridge::from_injector(injector);

        let err = bridge
            .inject(&[
                InputEvent::MouseMove { x: 0, y: 0 },
                InputEvent::MouseMove { x: 100, y: 100 },
                InputEvent::MouseMove { x: 200, y: 200 },
            ])
            .expect_err("首条投递失败必须让本批返回错误");
        assert!(
            err.to_string().contains("macOS inject failed"),
            "应保留既有包装错误: {err}"
        );
        assert_eq!(*calls.lock().expect("calls"), 3, "三条事件都必须尝试投递");
        assert_eq!(
            recorded.lock().expect("posted").len(),
            2,
            "失败后的兄弟事件必须照常投递"
        );
    }
}
