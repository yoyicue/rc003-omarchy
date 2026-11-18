# 按键与 Superwhisper 集成设计

## 1. 输入模型

RC003 在 Linux 配对后通过 `/dev/input/event*` 上报 HID。项目在内核 evdev 层只接管 RC003，再通过 uinput 输出需要的按键。Superwhisper 不需要知道遥控器存在，只看到普通 F9 和用户选择的 PipeWire 麦克风。

输入层必须保留三类语义：

- 瞬时事件：tap、double、hold、层切换。
- 持续事件：方向键重复、语音键 F9 的按下与松开。
- 清理事件：松键、断连、热插拔、退出和重置。

## 2. 设备识别与节点发现

参考固件的 HID 身份为：

- Vendor ID：`0x2717`
- Product ID：`0x32b8`

设备发现按以下顺序处理：

1. 遍历 evdev 节点并匹配 VID/PID。
2. 检查节点实际支持的 key codes，区分键盘、consumer control 和其他接口。
3. 对同一物理遥控器的多个节点建立统一设备组。
4. 名称仅用于诊断和辅助识别，不作为唯一安全边界。
5. 目标固件键码不一致时允许在配置中覆盖，但必须先用 `test-keys` 验证。

参考实现的 13 键记录如下，不能直接视为 RC003-MC 的实测结果：

| 逻辑键 | 参考 evdev code | 参考键名 |
|---|---:|---|
| 电源 | 116 | `KEY_POWER` |
| 语音 | 63；部分环境为 67 | `KEY_F5`；部分为 `KEY_F9` |
| 上 / 下 | 103 / 108 | `KEY_UP` / `KEY_DOWN` |
| 左 / 右 | 105 / 106 | `KEY_LEFT` / `KEY_RIGHT` |
| OK | 28 | `KEY_ENTER` |
| 返回 | 158 | `KEY_BACK` |
| 主页 | 102 | `KEY_HOME` |
| 菜单 | 127 | `KEY_COMPOSE` |
| TV | 41 | `KEY_GRAVE` |
| 音量 + / − | 115 / 114 | `KEY_VOLUMEUP` / `KEY_VOLUMEDOWN` |

## 目标设备按键实测

2026-11-18 对目标 RC003-MC 做了 45 秒临时独占探针，捕获到 13 个不同的 down 事件，13/13 键码均与上表一致，语音键确认是 `KEY_F5`（code 63）。当前只发现一个输入节点。探针使用临时 sudo 读取，结束后已释放 grab；尚未安装持久 udev 规则。

## 3. 独占与转发

- 只对 RC003 节点执行 `EVIOCGRAB`，绝不枚举后批量独占所有键盘。
- 创建项目专属的 uinput 虚拟设备，名称中明确包含 `rc003-omarchy`。
- 未映射且允许透传的按键保持原始 down/up/repeat；已消费事件不得再次透传。
- 独占失败、uinput 创建失败或节点权限不足时进入明确的 degraded 状态，不静默吞键。
- 退出时释放虚拟设备上仍按下的所有键，然后释放 evdev grab。
- 物理键盘的 F5/F9、RightAlt 和其他按键不经过本项目，保持原行为。

## 4. 语音键专用通道

语音键在普通映射之前处理：

```text
RC003 voice raw down → emit RightAlt down → Superwhisper starts native dictation
RC003 voice repeat   → ignore
RC003 voice raw up   → optional measured drain → emit F9 up → transcription and insertion
```

专用通道的规则：

- 不等待 tap/hold/double 判定，不增加手势延迟。
- 不经过宏、文本注入或 `superwhisper transcribe`。
- RightAlt/F9 down/up 必须一一对应；异常退出和断连时最多补发一次 F9 up。
- 默认 release drain 为 0，只有真机证明尾字被截断时才启用有限延迟。
- 语音键不支持普通单击、双击或长按动作，避免破坏听写生命周期。

这条通道是原生 Superwhisper 体验的必要条件。普通映射器的“松键后执行 key_stroke”不能替代它。

## 5. 其余 12 键

第一阶段先完成逐键识别和安全默认动作，再增加复杂映射。建议原则：

| 按键 | 初始行为原则 |
|---|---|
| 方向 | 保持方向键，支持系统重复 |
| OK | Enter；不在语音结束后自动提交 |
| 返回 | Backspace；应用控制层可另行映射 Escape |
| 音量 ± | 标准音量键或通过 PipeWire/Pulse 调整，不改默认设备 |
| 主页 / 菜单 / TV | 通过 Hyprland 验证后映射窗口、工作区或应用控制动作 |
| 电源 | 默认安全动作或禁用；不得直接关机、睡眠或关闭显示器 |

完整 13 键支持的含义是“每个键都能被识别、测试和配置”，不是为高风险按键预设不可撤销动作。

## 6. 进阶映射

在基础听写稳定后实现：

- `tap`、`hold`、`double`，阈值可配置且有上限。
- 基础层和临时/锁定层，层状态在超时、断连和退出时复位。
- 前台应用 profile，仅覆盖显式声明的动作，其他键继承全局配置。
- 宏由受限动作组成：按键、延迟、窗口动作、受控应用启动。
- 窗口/工作区操作优先使用 Hyprland IPC，普通键盘操作使用 uinput。
- 紧急恢复：长按菜单返回基础层并关闭浮层，释放所有持续动作。

参考硬件在 evdev 下表现为单键 rollover：按住一键时第二键可能不上报。因此默认配置不得依赖 OK+方向等同时按键手势；目标 RC003-MC 需要重新验证。

## 7. Superwhisper 配置

首次配置由用户确认：

1. 启动 `rc003-omarchy`，确认 PipeWire 源和 `superwhisper devices` 可见。
2. 在 Superwhisper 中选择 `RC003 Remote Microphone`。
3. 关闭“使用系统默认输入设备”或设置等价的设备 override；系统默认 source 保持不变。
4. 将 hold-to-talk 设置为 F9。
5. 在文本编辑器中按住遥控器语音键，确认原生录音界面出现并开始收音。
6. 松开后确认识别、上屏和焦点行为与使用键盘 F9 一致。

Redmi 实测的系统 hold 是 RightAlt。项目同时支持 F9 和 RightAlt，当前默认使用 RightAlt。实施时可以选择：

- 按 Omarchy 当前默认使用 RightAlt，界面与识别链路完全相同；或
- 将 Superwhisper hold 改为 F9 并同步修改项目配置。

不得在未告知用户的情况下修改这项现有快捷键。

## 8. 所有权边界

Superwhisper 独占以下职责：

- 录音状态和实时界面。
- 云端或离线模型选择。
- 识别、词典、历史、润色和文本上屏。
- 录音取消、焦点目标及剪贴板策略。

本项目只负责：

- 报告音频源是否可见、是否被选择。
- 提供实时音频和 RightAlt/F9 down/up。
- 在设备不在线、源未选择或快捷键不符合预期时给出诊断。

守护进程不调用 `superwhisper record/start/stop/transcribe` 来模拟原生路径，也不修改其模型、账户或许可配置。

## 9. 冲突与降级

- 不与 `rctool`、`mi-remote voice` 同时连接同一遥控器。
- 不与 `keyd`、`mi-remote keys run` 同时独占同一 evdev 节点。
- 不启用两套 PipeWire 虚拟麦克风。
- RC003 不在线时不自动改写 Superwhisper 的麦克风选择。
- 物理键盘 F9 仍会触发 Superwhisper，并使用其当前选择的 RC003 麦克风；这是设备 override 的预期行为，不是全局键映射泄漏。需要内置麦时由用户切换 Superwhisper 输入源。

## 10. 端到端验收

1. 按住语音键后，原生界面出现且目标窗口正确。
2. 说话时音频电平与内容对应，不是静音或上一段缓存。
3. 松开后只上屏一次，不自动按 Enter，不出现双重粘贴。
4. 连续五次听写没有 F9 卡键、会话错位或 BLE 重协商异常。
5. 遥控器断连、电脑休眠和服务退出后，F9 不会保持按下。
6. 普通键盘 F5/F9 和系统音量键不受项目接管影响。
7. 13 个实体键逐项通过测试后，才启用对应动作。

## 11. 输入节点缓存身份

非 RC003 节点按 `(路径, dev/inode)` 缓存，而不是只按 event 路径缓存，避免其他设备移除后，RC003 重连复用同一 eventN 编号时被永久忽略。只有缓存项存在且 dev/inode 匹配时才跳过；`stat` 失败与空缓存不会被误判为命中。
