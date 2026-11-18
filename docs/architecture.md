# 总体架构

## 1. 设计约束

第一版必须同时满足以下约束：

- 原生 Superwhisper 实时界面，而不是松开后调用文件转写。
- RC003 音频与按键都可以常驻，断线后能恢复。
- 普通键盘 F9、系统音频默认值和其他输入设备不被修改。
- 音频桥与输入映射可以分别诊断，但运行时使用一个进程协调设备代次和状态。
- 真机未通过的协议、按键或音频能力不得在界面上宣称成功。

## 2. 总体数据流

```text
                         控制面
        ┌──────────────────────────────────────────────┐
        │ pairing / discovery / capabilities / health │
        └──────────────────────────────────────────────┘
                           │
              ┌────────────┴────────────┐
              │ rc003-omarchy daemon    │
              │                         │
RC003 ATVV ───▶ BlueZ client            │
              │   └─▶ ATVV session      │
              │         └─▶ ADPCM       │
              │               └─▶ PCM ──┼──▶ PipeWire source ──┐
              │                         │                      │
RC003 HID ────▶ evdev input             │                      │
              │   └─▶ device filter     │                      │
              │         └─▶ mapper      │                      │
              │               ├─ F9 ────┼──▶ uinput ───────────┤
              │               └─ keys ──┼──▶ uinput / Hyprland │
              └─────────────────────────┘                      │
                                                              ▼
                                                     Superwhisper native UI
                                                  recognition and insertion
```

音频和按键在同一物理按键上产生，但逻辑上是两个独立输入。Superwhisper 只消费 PipeWire 音频与 F9 事件，不知道 ATVV、ADPCM 或遥控器实现。

## 3. 进程边界

采用一个用户级 `rc003-omarchy` 进程，理由如下：

- BLE 和 evdev 可以共享遥控器连接代次、在线状态和诊断信息。
- 避免两个进程争抢同一 BLE 会话或 evdev 独占句柄。
- 用户只需要管理一个服务、一个配置和一份日志。
- 进程内按任务隔离音频、输入和控制面，单个子系统失败不应拖死其他任务。

不采用“音频桥 + keyd + mi-remote + 自定义识别”多进程组合作为最终架构。开发阶段可以使用这些工具做对照实验，但不作为长期依赖。

## 4. 模块职责

| 模块 | 职责 | 明确不负责 |
|---|---|---|
| `discovery` | 查找已配对 RC003、核对名称/UUID、保存用户指定设备 | 不自动配对，不信任未确认设备 |
| `atvv` | 能力协商、MIC_OPEN/CLOSE、控制事件和会话状态机 | 不处理桌面快捷键 |
| `decode` | 帧累积、同步、IMA/DVI ADPCM 解码、PCM 后处理 | 不调用 ASR |
| `audio` | 创建 PipeWire 源、写入实时 PCM、处理欠载与背压 | 不修改默认音频设备 |
| `input` | 发现 RC003 event 节点、独占、热插拔、原始 down/up | 不读取其他键盘 |
| `mapping` | 语音键透传、13 键映射、层和动作状态机 | 不拦截 ATVV 音频 |
| `integrate` | Hyprland 窗口动作和 Superwhisper 就绪检查 | 不调用文件转写代替实时界面 |
| `config` | 校验 TOML、默认值、热加载边界 | 不保存凭据或语音内容 |
| `diagnostics` | `doctor`、计数器、状态和真机验收报告 | 不默认记录地址、文字或音频 |

## 5. 建议仓库结构

```text
src/
  main.rs              # CLI 入口和退出编排
  daemon.rs            # 任务生命周期、关闭和重启
  discovery.rs         # BlueZ 设备发现与身份
  atvv/
    mod.rs             # 会话状态机
    protocol.rs        # UUID、命令、能力帧和控制帧
  audio/
    mod.rs             # PipeWire 源生命周期
    decode.rs          # ADPCM 与帧累积
    pcm.rs             # 平滑、增益和格式转换
    buffer.rs          # 有界实时缓冲与统计
  input/
    mod.rs             # evdev 发现和热插拔
    device.rs          # 独占与 uinput 转发
    mapping.rs         # tap/hold/double、层、原始 F9 通道
    actions.rs         # 动作后端
  integrate/
    hyprland.rs        # 可选窗口/工作区动作
    superwhisper.rs    # 只读就绪检查与设备提示
  config.rs
  diagnostics.rs
tests/
  fixtures/            # 不含真实语音的协议与合成 PCM 样本
docs/
systemd/
udev/
```

这是逻辑边界，实施时可合并小文件，但不得把协议解析、实时音频回调和桌面动作揉成同一个函数。

## 6. 并发与实时约束

- 使用 Tokio 管理 BLE、设备发现和生命周期任务。
- PipeWire 回调线程不执行 BLE、日志格式化、堆分配、锁等待或桌面 IPC。
- BLE 到音频回调使用有界队列；队列满时保实时性并记录丢弃计数，不允许无限增加延迟。
- 输入映射单独处理 evdev fd；F9 down/up 不等待音频任务。
- 每次物理连接分配单调递增的 generation。旧 generation 的回调、定时器和断开事件不得污染新会话。
- 所有任务由统一 cancellation token 关闭；退出时必须释放 F9、其他按住键、evdev grab 和 PipeWire 流。

## 7. 状态划分

```text
Daemon:   starting → ready → degraded → stopping
BLE:      disconnected → discovering → negotiating → ready → streaming
Audio:    unavailable → silent → streaming → draining
Input:    absent → capturing → degraded
Mapping:  base layer ↔ optional layer
```

`ready` 只表示守护进程自身可运行。UI 必须分别显示 BLE、PipeWire、输入权限和 Superwhisper 状态，不能用一个“已连接”掩盖降级链路。

## 8. 技术选型

- Rust：适合实时音频、单一二进制和长期用户服务。
- `bluer`：ATVVoice 已用于 BlueZ D-Bus，优先复用其连接模式。
- `pipewire` crate：创建真正的 `Audio/Source`，避免 CPAL/ALSA 与 `pactl` 设备枚举不一致。
- evdev/uinput：在桌面环境之前处理输入，保持按下/松开语义并隔离设备。
- TOML：显式、严格、适合少量用户配置；未知字段默认拒绝。
- Hyprland IPC：仅用于窗口和工作区动作；普通键盘事件优先走 uinput。

具体 crate 版本在实现时通过构建和最小验证确定，不在设计阶段虚构锁定。

## 9. 方案取舍记录

| 方案 | 结论 | 原因 |
|---|---|---|
| `rctool` 作为底座 | 不采用 | Linux 无按键层；CPAL/ALSA 与 `pactl` 虚拟设备之间存在额外兼容风险。 |
| `mi-remote-linux` 直接运行 | 不作为最终方案 | 自带 ASR 与上屏，绕过 Superwhisper 原生界面；映射器不适合原始 F9 保持。 |
| ATVVoice 直接运行 | 先做对照验证 | 原生 PipeWire 路径正确，但 RC003 未在其官方支持表中验收。 |
| `keyd` 作为最终依赖 | 不作为长期依赖 | 简单映射可用，但项目需要完整 13 键、诊断和统一生命周期。 |
| 新 Rust 项目 | 采用 | 可以只保留需要的音频、输入和 Superwhisper 集成，并建立真机门禁。 |

## 10. 复用与许可

- 可参考并复用 MIT 许可的 ATVVoice、mi-remote-linux，保留版权和许可文本。
- `rctool` 是 GPL-3.0-only，本阶段只参考行为，不复制源码。
- 新仓库尚未选择公开许可证；在决定分发或许可策略前，不混入不兼容代码。

## 11. 参考实现模块映射

三个参考仓库位于 Redmi 的 `~/Work/tries/`。ATVVoice 当前参考提交为 `0e8f641`，采用 MIT 许可。

| 目标模块 | 参考位置 | 使用方式 |
|---|---|---|
| PipeWire 虚拟源 | `atvvoice/src/pw.rs:41` | 参考其独立线程、流创建、PCM 写入和销毁生命周期，调整为 RC003 的 16 kHz 单声道。 |
| `Audio/Source` 节点 | `atvvoice/src/pw.rs:80` | 复用节点类型和属性设计，不创建系统默认设备。 |
| 单声道位置 | `atvvoice/src/pw.rs:204` | 必须显式设置 MONO，避免只在左声道出声。 |
| 输出方向 | `atvvoice/src/pw.rs:220` | `Direction::Output` 表示向 PipeWire 输出，对外呈现为麦克风。 |
| ATVV 设备发现 | `atvvoice/src/ble.rs:145` | 参考 BlueZ D-Bus 发现和已配对设备处理。 |
| GATT 特征解析 | `atvvoice/src/ble.rs:191` | 参考 TX/RX/CTL 解析和 AcquireNotify 生命周期。 |
| 会话状态机 | `atvvoice/src/atvv.rs:111` | 参考任务边界、停止和超时，不照搬其他遥控器的命令或帧格式。 |
| CLI 与重连 | `atvvoice/src/main.rs:177` | 参考 CLI、重连退避、信号处理和统一退出。 |
| RC003 ATVV | `mi-remote-linux/src/mi_remote_linux/atvv.py` | 以 RC003 的 GET_CAPS、能力解析和命令定义为主要协议依据。 |
| RC003 帧与 ADPCM | `mi-remote-linux/src/mi_remote_linux/adpcm.py` | 移植 120/126 字节帧处理和 16 kHz 解码，并转换现有测试。 |
| RC003 13 键 | `mi-remote-linux/src/mi_remote_linux/hid_engine.py` | 使用已与目标硬件逐项核对的键码表。 |
| Rust 行为参考 | `rctool/crates/rctool-core/src/` | 只研究状态机和 DSP 行为，不复制 GPL 源码，不采用其 Linux CPAL/ALSA 音频路径。 |

### PipeWire 线程模型

ATVVoice 的关键结构是 BLE/ATVV 在 Tokio 任务中运行，解码后的 PCM 经有界通道交给独立 OS 线程中的 PipeWire main loop。`pipewire-rs` 不作为 Tokio 任务运行。新项目保留这一边界，但通道必须有界，且不得让回调线程等待 BLE、日志或桌面 IPC。

### 协议参数不可混用

| 项目 | RC003-MC 目标基线 | ATVVoice 文档中的其他遥控器示例 |
|---|---|---|
| 协议版本 | 已实测 `0x0100` | v0.4 / v1.0，按设备协商 |
| 采样率 | 已确认 16 kHz | 示例包含 8 kHz |
| 音频帧 | 已确认 120 字节 | 示例包含 134 字节带头帧 |
| ADPCM 状态 | 按 RC003 参考实现和 SYNC 处理 | 部分设备要求逐帧从头部重置 |
| MIC_OPEN | 采用 RC003 参考实现，首次真实语音流仍需验收 | 其他设备可能需要不同 codec 参数字节 |

因此，ATVVoice 主要提供 **PipeWire 和生命周期实现**，不是 RC003 协议参数的权威来源。RC003 的协议与解码以 `mi-remote-linux` 及本机实测为准。
