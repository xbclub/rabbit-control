//! 输入注入器（仅 macOS）。
//!
//! [`Injector`] = 权限预检 + 坐标钳制 + 投递。泛型 [`EventPoster`] 让
//! 未授权环境（CI、单测）也能用假投递器验证全部逻辑，只有真投递
//! （`#[ignore]` 的 [`inject_moves_cursor`]）需要辅助功能授权。

use crate::error::InputError;
use crate::event::InputEvent;
use crate::ffi::CgPoster;
use crate::plan::{self, EventPoster, ScreenBounds};

/// 把远控端输入事件注入本机的注入器。
///
/// # 权限前置（T12 M2）
/// `CGEventPost` 未授权时**静默丢弃事件**（不报错）。因此 [`Injector::new`]
/// 构造时先 `AXIsProcessTrusted()` 预检，未授权返回 [`InputError::NotTrusted`]
/// （附系统设置路径），绝不静默失败。授权在系统设置里打开后**必须重启
/// 进程**才生效（macOS 限制，T12 M4）。
///
/// # 用法
/// ```no_run
/// # fn demo() -> Result<(), rc_input_macos::InputError> {
/// let mut inj = rc_input_macos::Injector::new()?;
/// inj.inject(rc_input_macos::InputEvent::PointerMove { x: 100.0, y: 100.0 })?;
/// # Ok(())
/// # }
/// ```
pub struct Injector<P: EventPoster = CgPoster> {
    poster: P,
    screen: ScreenBounds,
}

impl Injector<CgPoster> {
    /// 生产构造：辅助功能预检 → `CGEventSource`（HIDSystemState）→
    /// 屏幕联合边界。
    ///
    /// 未授权时立即返回 [`InputError::NotTrusted`]，不会创建任何 CG 资源。
    pub fn new() -> Result<Self, InputError> {
        if !crate::ffi::ax_is_process_trusted() {
            return Err(InputError::NotTrusted);
        }
        let poster = CgPoster::new()?;
        let screen = crate::ffi::screen_bounds();
        Ok(Self { poster, screen })
    }
}

impl<P: EventPoster> Injector<P> {
    /// 显式组装（测试 / 上层已自行检测过授权时用）。
    ///
    /// `trusted = false` 时立即返回 [`InputError::NotTrusted`]——这是
    /// 未授权错误路径的可测入口：不创建 CG 资源、不投递任何事件。
    pub fn from_parts(poster: P, screen: ScreenBounds, trusted: bool) -> Result<Self, InputError> {
        if !trusted {
            return Err(InputError::NotTrusted);
        }
        Ok(Self { poster, screen })
    }

    /// 当前使用的屏幕边界（坐标钳制依据，构造时采样）。
    ///
    /// 注意：热插拔显示器后边界可能过期，上层应重建注入器。
    pub fn screen_bounds(&self) -> ScreenBounds {
        self.screen
    }

    /// 注入一条输入事件：翻译（坐标钳制 + 滚动取整 + 键码映射）→ 投递。
    pub fn inject(&mut self, event: InputEvent) -> Result<(), InputError> {
        let planned = plan::plan(&event, self.screen)?;
        self.poster.post(&planned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::PointerButtonKind;
    use crate::keymap::Key;
    use crate::plan::{PlannedEvent, PlannedMouseKind};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// 假投递器：只记录计划事件，不触碰系统（共享状态便于注入器
    /// 持有所有权后仍能断言）。
    #[derive(Clone, Default)]
    struct FakePoster {
        posted: Rc<RefCell<Vec<PlannedEvent>>>,
    }

    impl EventPoster for FakePoster {
        fn post(&mut self, planned: &PlannedEvent) -> Result<(), InputError> {
            self.posted.borrow_mut().push(*planned);
            Ok(())
        }
    }

    /// 未授权：构造即失败，错误信息能指导用户去系统设置，
    /// 且全程没有事件被投递（防静默失败的回归测试）。
    #[test]
    fn untrusted_rejects_before_any_post() {
        let screen = ScreenBounds::new(0.0, 0.0, 1920.0, 1080.0);
        let recorded = Rc::new(RefCell::new(Vec::new()));
        let err = match Injector::from_parts(
            FakePoster {
                posted: Rc::clone(&recorded),
            },
            screen,
            false,
        ) {
            Err(e) => e,
            Ok(_) => panic!("未授权时构造必须失败，不能静默成功"),
        };
        assert_eq!(err, InputError::NotTrusted);
        let msg = err.to_string();
        assert!(msg.contains("辅助功能"), "错误信息必须提到辅助功能：{msg}");
        assert!(msg.contains("系统设置"), "错误信息必须指向系统设置：{msg}");
        assert!(recorded.borrow().is_empty(), "未授权时不得投递任何事件");
    }

    /// 已授权（模拟）：注入经假投递器记录，坐标被钳制、键码被翻译。
    /// 全链路不调用任何 CG API。
    #[test]
    fn inject_via_fake_poster_records_clamped_events() {
        let screen = ScreenBounds::new(0.0, 0.0, 100.0, 100.0);
        let recorded = Rc::new(RefCell::new(Vec::new()));
        let poster = FakePoster {
            posted: Rc::clone(&recorded),
        };
        let mut inj = Injector::from_parts(poster, screen, true).unwrap();

        // 越界坐标被钳到边界
        inj.inject(InputEvent::PointerMove { x: -50.0, y: 50.0 })
            .unwrap();
        // 键码映射：Key::Escape = kVK_Escape = 0x35
        inj.inject(InputEvent::Key {
            key: Key::Escape,
            pressed: true,
        })
        .unwrap();
        drop(inj);

        let posted = recorded.borrow();
        assert_eq!(posted.len(), 2, "两条事件都应投递");
        assert_eq!(
            posted[0],
            PlannedEvent::Mouse {
                kind: PlannedMouseKind::Moved,
                button: PointerButtonKind::Left,
                x: 0.0,
                y: 50.0,
            }
        );
        assert_eq!(
            posted[1],
            PlannedEvent::Key {
                keycode: 0x35,
                pressed: true,
            }
        );
    }

    /// 真机验证：注入的 `PointerMove` 真的进入系统事件流。
    /// 需要辅助功能授权（`AXIsProcessTrusted == true`）。
    ///
    /// 断言手段（tap 见证）：注入一个到**独特目标位**的移动，ListenOnly
    /// CGEventTap 必须捕获到该坐标的 MouseMove。**不用**光标最终位置或
    /// HID 计数器断言——两版前车之鉴存档：
    ///
    /// - `CGEventSourceCounterForEventType(HIDSystemState)`：合成事件只进
    ///   Session 态，HID 计数不涨（偶尔通过纯属窗口内混入真实硬件移动）；
    /// - Session 态光标位置：本机有活的物理输入（用户的手在鼠标/触控板
    ///   上），系统会在合成移动后立即重锚到物理位置，「停在目标位」
    ///   物理上不可保证。
    ///
    /// tap 见证与套件其余部分（回环/延迟探针）同一判定面：事件流语义
    /// 正确、坐标正确，最终像素位归 OS 与物理输入博弈。
    #[test]
    #[ignore = "requires macOS + Accessibility granted (系统设置 → 隐私与安全性 → 辅助功能)"]
    fn inject_moves_cursor() {
        // 真机测试串行化（见 crate::rm_lock 文档）。
        let _rm = crate::rm_lock::REAL_MACHINE
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut inj = Injector::new().expect(
            "需要辅助功能授权：系统设置 → 隐私与安全性 → 辅助功能，\
             允许后重启测试进程再跑 --ignored",
        );
        use rc_input::InputCapture as _;
        let mut capture = crate::capture::CgCapture::new()
            .expect("CgCapture::new 需要辅助功能授权（系统设置 → 隐私与安全性 → 辅助功能）");
        // 捕获与注入共享同一联合边界（归一化定义域一致的前提）。
        let screen = inj.screen_bounds();
        assert_eq!(capture.screen_bounds(), screen);

        // 候选目标都在屏幕内腹。避开当前光标位（零位移移动可能被系统
        // 合并），从与光标位差异最大的候选里选，进一步降低与物理输入
        // 撞车的概率。
        let current = crate::ffi::current_cursor_location();
        let mut candidates = [
            (
                screen.x + screen.width * 0.5,
                screen.y + screen.height * 0.5,
            ),
            (
                screen.x + screen.width * 0.25,
                screen.y + screen.height * 0.25,
            ),
            (
                screen.x + screen.width * 0.75,
                screen.y + screen.height * 0.75,
            ),
            (
                screen.x + screen.width * 0.25,
                screen.y + screen.height * 0.75,
            ),
        ];
        candidates.sort_by_key(|(x, y)| {
            let d = (*x - current.0).abs() + (*y - current.1).abs();
            std::cmp::Reverse(d as u64)
        });
        let target = candidates[0];

        // 静置：清掉测试启动前残留在通道里的事件。
        let _ = capture
            .next_events(std::time::Duration::from_millis(200))
            .unwrap();
        inj.inject(InputEvent::PointerMove {
            x: target.0,
            y: target.1,
        })
        .expect("inject PointerMove");

        // 期待值：目标位的归一化坐标（与捕获侧同一 normalize 定义）。
        let bounds = capture.screen_bounds();
        let (want_x, want_y) = crate::capture::normalize_point(target.0, target.1, bounds);
        // CGEventPost 异步生效；轮询捕获侧最多 1s。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            let evs = capture
                .next_events(std::time::Duration::from_millis(50))
                .unwrap();
            let hit = evs.iter().any(|e| match e {
                rc_input::InputEvent::MouseMove { x, y } => {
                    (i32::from(*x) - i32::from(want_x)).unsigned_abs() <= 100
                        && (i32::from(*y) - i32::from(want_y)).unsigned_abs() <= 100
                }
                _ => false,
            });
            if hit {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!(
                    "注入 (≈{want_x},{want_y}) 的 mouseMoved 1s 内未被 tap 捕获\
                     ——事件被系统静默丢弃，通常是辅助功能授权未生效"
                );
            }
        }
    }
}
