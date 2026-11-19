# RC003 Omarchy 设计文档

## 项目定位

`rc003-omarchy` 是面向 Omarchy 的小米蓝牙遥控器 2 Pro（RC003）集成工具。项目不提供另一套语音识别器，而是把 RC003 的音频和按键接入系统已有的 Superwhisper，保留其原生按住听写界面、模型、词典和上屏行为。

当前仓库已完成第一版实现，以及真实听写、服务启动和断线自动恢复验收。文档同时记录设计依据、已实现边界和未通过的验收项，不把自动测试或设备枚举等同于完整用户流程通过。

## 目标

1. 将 RC003 的 ATVV 语音实时暴露为原生 PipeWire 麦克风。
2. 将遥控器语音键转换为保真的 F9 按下/松开事件，触发 Superwhisper 原生听写。
3. 支持 RC003 的 13 个实体键，后续扩展单击、长按、双击、层、宏和前台应用配置。
4. 只接管 RC003，不修改物理键盘、全局快捷键、系统默认麦克风或默认扬声器。
5. 默认不保存音频，不在守护进程内上传音频，不静默切换 Superwhisper 的本地或云端模式。
6. 提供可重复的诊断、真机验收、断线恢复和用户级服务部署。

## 非目标

- 不移植 SayAll 的 macOS UI 或商业功能。
- 不实现新的 ASR、LLM 润色或文本上屏引擎。
- 不追求通用蓝牙遥控器框架；先完成 RC003-MC 在 Redmi/Omarchy 上的验收。
- 不同时支持多个第三方听写应用；第一版只针对 Superwhisper。
- 不通过 Wine、Windows VM 或 ESP32 增加运行链路。
- 不在第一版提供任意 shell 命令动作或云端同步。

## 已确认事实与待验证假设

| 类型 | 内容 |
|---|---|
| 已确认 | Redmi 是 x86_64 Omarchy，已有 BlueZ、PipeWire、Fcitx5 和 Superwhisper 1.3.1。 |
| 已确认 | RC003 使用 BLE ATVV 传输语音，同时通过 HID 上报按键；语音键不是普通系统麦克风。 |
| 已确认 | 参考实现记录了 RC003 的 16 kHz IMA/DVI ADPCM、ATVV UUID 和 13 键 evdev 键码。 |
| 已确认 | `rctool` 的 Linux 按键映射未实现；`mi-remote-linux` 的语音直接进入自有 ASR，不提供通用虚拟麦克风。 |
| 已确认（2026-11-18 真机） | RC003-MC 固件 00A4；ATVV v1.0、16 kHz codec、120 字节帧；13/13 键码与参考一致，语音键为 F5。 |
| 已确认（Redmi 真机） | PipeWire 虚拟源可被 Superwhisper 枚举和选择，真实语音已完成识别与上屏。 |
| 已确认（Redmi 真机） | RightAlt/F9 down/up 能驱动原生听写；默认零 release drain 已通过真实听写。 |
| 已确认（Redmi 真机） | 已收到真实 AUDIO_START/AUDIO_STOP 和 PCM，并完成遥控器断线后的自动恢复。 |
| 待验证 | RC003-MC 同时按多个实体键时的 rollover 行为。 |

## 文档索引

- [总体架构](architecture.md)：进程边界、模块、数据流、技术选型和仓库结构。
- [ATVV 与音频桥](audio-and-protocol.md)：配对、协议、解码、PipeWire、缓冲和重连。
- [按键与 Superwhisper](input-and-superwhisper.md)：13 键、F9 原生听写、映射和冲突隔离。
- [配置、部署与运维](configuration-and-operations.md)：配置、权限、服务、日志和故障恢复。
- [安全与隐私](security-and-privacy.md)：权限边界、数据边界和供应链约束。
- [测试与实施路线](testing-and-roadmap.md)：自动测试、真机门禁、里程碑和完成定义。

## 核心决策摘要

- 使用单个 Rust 用户态守护进程，不安装内核模块，不运行系统级服务。
- 使用 BlueZ/ATVV 获取音频，使用原生 PipeWire 源输出实时 PCM。
- 使用 evdev/uinput 接管并映射 RC003；语音键直接转发为 F9 down/up，不经过手势识别器。
- Superwhisper 是唯一的听写、识别和文本上屏所有者。
- 优先复用 MIT 许可的 ATVVoice 与 mi-remote-linux 的实现经验；`rctool` 仅作行为参考，不复制其 GPL 源码。
