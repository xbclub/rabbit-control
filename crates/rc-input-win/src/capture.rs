//! 捕获骨架（仅 Windows；真机实现属 D 阶段）。
//!
//! 声明 `WH_KEYBOARD_LL` / `WH_MOUSE_LL` 低层钩子 API 与
//! [`MSLLHOOKSTRUCT`] 布局；捕获器 [`LowLevelHookCapture`] 实现了
//! `rc_input::InputCapture` 的**错误路径骨架**。

// extern 声明集中在骨架模块（任务卡注记：安全 API 声明）。
#![allow(unsafe_code)]
// 骨架：常量与结构供 D 阶段实现使用，当前仅声明未引用。
#![allow(dead_code)]

use rc_input::{InputCapture, InputEvent};
use rc_types::{Error, Result};
use std::time::Duration;

/// 低层键盘钩子（winuser.h）。
pub const WH_KEYBOARD_LL: i32 = 13;
/// 低层鼠标钩子（winuser.h）。
pub const WH_MOUSE_LL: i32 = 14;

/// 钩子过程签名（`LRESULT CALLBACK LowLevelProc(int, WPARAM, LPARAM)`）。
pub type HOOKPROC =
    Option<unsafe extern "system" fn(code: i32, wparam: usize, lparam: isize) -> isize>;

/// `MSLLHOOKSTRUCT`（低层鼠标钩子的载荷）。
#[repr(C)]
#[allow(non_snake_case)]
pub struct MSLLHOOKSTRUCT {
    /// 事件坐标（虚拟桌面像素，可为负——副屏负象限，坑 3）。
    pub pt: POINT,
    /// 滚轮量 / X 键编号（若适用）。
    pub mouseData: u32,
    /// 标志位（含 `LLMHF_INJECTED`——注入事件自带标记，是系统事实）。
    pub flags: u32,
    /// 事件时间戳。
    pub time: u32,
    /// 附加信息。
    pub dwExtraInfo: usize,
}

/// 屏幕点（与 `inject::POINT` 同布局，按模块自持声明）。
#[repr(C)]
#[allow(non_snake_case)]
pub struct POINT {
    /// x 坐标（像素）。
    pub x: i32,
    /// y 坐标（像素）。
    pub y: i32,
}

#[link(name = "user32")]
extern "system" {
    /// `SetWindowsHookExW`：挂低层钩子。
    pub fn SetWindowsHookExW(
        idhook: i32,
        lpfn: HOOKPROC,
        hmod: *mut core::ffi::c_void,
        dwthreadid: u32,
    ) -> *mut core::ffi::c_void;
    /// `UnhookWindowsHookEx`：摘钩子。
    pub fn UnhookWindowsHookEx(hhk: *mut core::ffi::c_void) -> i32;
}

/// 低层钩子捕获器骨架（D 阶段实现）。
///
/// 当前 [`InputCapture::next_events`] 恒返回 `Err`——骨架不假装能捕获。
pub struct LowLevelHookCapture {}

impl LowLevelHookCapture {
    /// 构造骨架。
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for LowLevelHookCapture {
    fn default() -> Self {
        Self::new()
    }
}

impl InputCapture for LowLevelHookCapture {
    fn next_events(&mut self, _timeout: Duration) -> Result<Vec<InputEvent>> {
        // TODO(D 阶段)：
        // 1. 专用线程挂 WH_MOUSE_LL + WH_KEYBOARD_LL，跑 GetMessage 循环
        //    （任务卡坑 5：钩子回调依赖消息循环，别挂主线程）。
        // 2. 回调里只入队（锁/无锁环形缓冲），把事件转成归一化
        //    InputEvent（多显示器负坐标 → 虚拟桌面归一化，坑 3/6）。
        // 3. 本机注入的事件（LLMHF_INJECTED）必须丢弃，否则远控回环
        //    会把注入再捕获一遍（回声）。
        // 4. 键盘用扫描码（KBDLLHOOKSTRUCT.scanCode，I5）；松键丢失时
        //    上层批处理器的「全松」兜底负责（坑 7）。
        Err(Error::InvalidConfig(
            "WH_MOUSE_LL/WH_KEYBOARD_LL capture not implemented (D-phase)".into(),
        ))
    }
}
