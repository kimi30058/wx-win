//! wxauto-desktop 核心库（spec §3.2 sidecar 协议层 + §2.3 微信能力域 + §3.1 WS 传输层 + §5.1 编排层 + §3.3 状态机/配置）
//!
//! GUI 装配层（commands.rs / ui_events.rs / app_state.rs）依赖 tauri，
//! 挂在 bin（main.rs 的 mod 声明）——库本体保持 core 纯净
//! （sidecar/wx/transport/agent_link/state 不引 tauri）。bin 单测
//! （cargo test 默认含 bin target）覆盖 GUI 装配的纯逻辑部分。
pub mod agent_link;
pub mod bootstrap;
pub mod cli;
pub mod config;
pub mod outbox;
pub mod sidecar;
pub mod state;
pub mod transport;
pub mod wx;
