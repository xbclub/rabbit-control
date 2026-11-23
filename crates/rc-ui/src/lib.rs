//! RabbitControl 控制端 UI 库模块（T21 M2-6）。
//!
//! 将不依赖 Tauri 窗口事件循环的纯逻辑与桥接组件导出为库，便于离线确定性单测与
//! 进程内集成测试；全 crate 默认 `#![deny(unsafe_code)]`，仅 [`metal_attach`] 允许受控 `unsafe`。

#![deny(unsafe_code)]
#![deny(missing_docs)]

pub mod app_prefs;
pub mod auth_client;
pub mod cert_manager;
pub mod display_ctrl;
pub mod host_bridge;
pub mod hud;
pub mod input_bridge;
pub mod lan_discovery;
pub mod metal_attach;
pub mod model;
pub mod p2p;
pub mod permissions;
pub mod relay_transport;
pub mod server_config;
pub mod session_bridge;
pub mod signal_client;
