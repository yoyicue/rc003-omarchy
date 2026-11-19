# 配置、部署与运维设计

## 1. 配置原则

- 默认配置优先位于 `~/.config/rc003-omarchy/config.toml`。
- 配置只包含设备、音频、输入和日志设置，不保存 Superwhisper 凭据、语音文本或 API key。
- 未知字段、错误类型和越界值拒绝启动，避免拼写错误被静默忽略。
- 显式传入 `--config PATH` 时，文件不存在也直接报错；只有未显式指定路径时才允许使用内置默认值。
- 地址等敏感标识只保留在本机，文档和测试 fixtures 使用示例值。
- 安全的热更新仅覆盖增益和日志级别；设备、PipeWire 节点和输入映射变化要求重启服务。

示例结构，不包含真实设备地址：

```toml
version = 1

[device]
address = ""
auto_connect = true
reconnect_initial_ms = 1000
reconnect_max_ms = 15000

[audio]
node_name = "rc003-omarchy"
description = "RC003 Remote Microphone"
gain_db = 0.0
release_drain_ms = 0
frame_layout = "raw"

[input]
enabled = true
voice_key = "RightAlt"
release_grab_on_exit = true

[mapping]
enabled = true
profile = "default"

[logging]
level = "info"
```

`audio.frame_layout` 默认为 `raw`，按 CAPS 协商的帧长立即切分裸音频；只有确认固件携带 6 字节同步头时才设为 `headered`。旧配置缺少此字段时仍按 `raw` 读取。不要根据单次通知长度自动猜测格式，因为 126 字节也可能只是裸帧的一个分片。

最终字段以实现的严格 schema 为准；实现不得悄悄增加云端端点或录音保存路径。

## 2. CLI 设计

```text
rc003-omarchy scan
rc003-omarchy doctor [--json]
rc003-omarchy test-keys
rc003-omarchy test-audio
rc003-omarchy test-dictation
rc003-omarchy run [--config PATH] [--verbose]
rc003-omarchy service show
rc003-omarchy service install --apply
rc003-omarchy service status
rc003-omarchy service uninstall
```

约束：

- `doctor` 只读，不连接 BLE、不独占输入、不修改配置。
- `test keys` 独占 RC003 后只显示事件，不执行电源、锁屏或窗口动作。
- `test dictation` 使用用户明确聚焦的测试输入框，不自动按 Enter。
- `--device ADDRESS` 同时约束 BLE 音频设备和 evdev 按键设备；evdev 通过 `unique_name()` 匹配地址，避免两只 RC003 混用麦克风与按键。
- `service install/uninstall` 默认预览；只有 `--apply` 才写文件或调用 systemctl。
- 工具本身不调用 sudo。需要系统规则时输出内容，由用户审查后单独安装。

## 3. 权限与 udev

运行时权限分为三部分：

| 能力 | 机制 | 备注 |
|---|---|---|
| BlueZ/ATVV | 用户会话 D-Bus + 已配对设备 | 不需要 root |
| PipeWire 源 | 当前用户 PipeWire 会话 | 必须在图形用户会话内 |
| evdev/uinput | 最小化 udev 规则或发行版既有 input 权限 | 需要读取 RC003 并创建虚拟输入 |

udev 规则应只匹配 RC003 的事件节点；uinput 规则可授权 `input` 组。项目规则安装为 `/etc/udev/rules.d/70-rc003-omarchy.rules`，在 systemd 的 `73-seat-late.rules` 之前添加 `uaccess`。加入 `input` 组会扩大用户读取输入设备的能力，安装前必须向用户说明，不能作为无提示副作用。

权限未就绪时：

- 音频桥可以单独运行和测试。
- 输入层显示 degraded，不假装 13 键可用。
- 不使用强制独占共享节点的方式掩盖 uinput 缺失。

## 4. systemd 用户服务

服务原则：

- 使用 `systemd --user`，不安装系统级 daemon。
- `After=graphical-session.target pipewire.service`，并随图形会话退出。
- `Restart=on-failure`，使用有限重启间隔，避免持续错误刷日志。
- 使用绝对可执行路径和显式配置路径；通过 `newgrp input -c` 启动，使服务进程获得输入组权限。
- 安装时把当前二进制复制到 `~/.local/bin/rc003-omarchy`，避免 unit 指向 `target/` 构建目录；可执行文件和配置路径仅允许 ASCII 字母、数字、`.`、`_`、`/`、`-`，特殊字符直接拒绝，避免 systemd 与 shell 双层转义。覆盖安装后用一次 `restart` 启动新二进制，避免首次安装重复启动。
- PipeWire 启动握手失败或运行中异常会使主进程失败退出，由 `Restart=on-failure` 处理，不保留“服务运行但没有麦克风”的假健康状态。
- 退出时停止所有子任务并释放输入、F9 和音频资源。
- 自定义 `--config` 路径原样写入 unit；默认配置文件存在时写入其路径，不存在时省略 `--config` 并使用内置默认值，避免首次安装生成无法启动的服务。
- 默认只在用户明确安装并启用后常驻，不在首次构建时自动启动。

服务状态必须区分：未安装、未启用、运行中、 degraded、权限失败和配置失败。

## 5. 日志与诊断

默认写入 journald 或 stderr，级别为 info。默认日志允许包含：

- 生命周期、协议版本、采样率、帧长和会话计数。
- 连接/断开原因类别、重连次数和 generation。
- 音频帧数、underrun、overflow、丢弃样本和当前缓冲水位。
- evdev 节点类别、权限失败和映射动作类别。

默认日志不得包含：

- 语音 PCM、WAV 路径、识别文本、剪贴板内容。
- 完整蓝牙地址、个人路径、账户信息或 Superwhisper 凭据。
- 其他前台应用的文本内容。

调试模式可以记录更详细的协议元数据，但不能把“调试”变成默认保存语音。若未来增加录音诊断，必须使用独立显式命令、私有目录和用完即删提示。

## 6. 升级与配置迁移

- Cargo.lock 纳入版本控制，发布构建可重复。
- 配置包含 `version`；迁移前创建私有备份，迁移失败保留原文件。
- 新版本先运行 `doctor` 和前台测试，再替换用户服务。
- 第一版不做自动更新，不执行 `curl | sh` 安装。
- 卸载服务时保留配置；是否删除配置和模型由用户另行决定。

## 7. 常见故障路径

| 症状 | 首要检查 |
|---|---|
| 找不到遥控器 | 配对/trust、遥控器唤醒、ATVV service、是否被其他桥接占用 |
| 已连接但无音频 | CAPS、codec、AUDIO_START、PipeWire 源、Superwhisper 输入选择 |
| 有音频但无界面 | RC003 evdev 权限、F9 down/up、Superwhisper hold 配置 |
| 首字丢失 | 源启动延迟、首帧前导、Superwhisper 开始读取时间 |
| 尾字丢失 | AUDIO_STOP 尾部包、F9 release drain、缓冲清理顺序 |
| 按键重复或无效 | 多进程独占、uinput 重复转发、应用 profile |
| 重启后失效 | 用户图形会话、PipeWire 顺序、udev 规则和设备重连 |

## 8. 重连退避

重连使用有上限的指数退避。协议创建失败和不支持的采样率在重试前都会等待；成功会话结束后将退避重置为初始值，避免长期运行时退避时间持续增大。
