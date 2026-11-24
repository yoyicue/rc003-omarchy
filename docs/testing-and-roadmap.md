# 测试与实施路线

## 当前实现与验收状态（2026-11-20）

- M0：完成。Rust 工具链、构建、配置和只读诊断均可用。
- M1：完成。目标 RC003-MC 已验证配对、ATVV v1.0、16 kHz、120 字节帧及 13/13 键码。
- M2：完成。PipeWire 源已由 Superwhisper 枚举，真实语音已通过识别与上屏验证。
- M3：完成。RightAlt down/up 和真实 ATVV 音频均已验证，Superwhisper 中产生三条真实识别记录。
- M4：实现完成，13 键及 RightAlt 已通过真机验证。
- M5：完成。服务已通过 `newgrp input -c` 安装并启用，启动权限和遥控器断线后的自动恢复均已通过验收。

此前 Redmi 基线：140 个单元测试、显式 uinput 集成测试、Clippy（warnings denied）和 release build 全部通过。第九、十轮分别有 123、124 项可移植测试通过；第十一至十三轮的 Redmi 单元测试数分别为 148、149、149，详见第 18、19、20 节。第十四至十六轮分别有 126 项可移植测试通过；第十七至二十一轮分别有 153、156、157、158 个单元测试在 Redmi 通过，详见第 24、25、26、27、28 节。


## 1. 验证原则

- 单元测试证明协议和状态机，不证明真实硬件可用。
- PipeWire 测试证明虚拟音频链路，不证明 RC003 的语音质量。
- Superwhisper 测试必须使用真实按键、真实音频和真实输入框。
- 每个里程碑先通过门禁再常驻，不把调试中的服务直接设为开机启动。
- 真机结果记录固件、键码、能力和失败类别，不记录语音正文。

## 2. 自动化测试

### 协议与解码

- GET_CAPS/CAPS_RESP、不同版本和非法能力帧。
- MIC_REQUEST、OPEN、START、SYNC、STOP、CLOSE 顺序。
- 120/126 字节帧、跨包拼接、截断、错位重同步。
- ADPCM 跨批状态、SYNC 重置、连续会话隔离。
- 旧 generation 的通知和定时器不得影响新会话。

### 音频

- PCM 格式、增益、削顶、重采样和静音填充。
- 有界队列 overflow、underrun、清理和延迟上限。
- PipeWire 节点属性、创建/销毁和默认 source 不变。
- 会话结束后无旧样本重放。

### 输入

- VID/PID 过滤、多个 event 节点、热插拔和独占失败。
- F9 down/up、repeat 忽略、断连补 up 和退出清理。
- tap/hold/double、层超时、profile 继承和紧急复位。
- 同一事件不会被同时透传和执行映射。

### 配置与部署

- TOML 未知字段、类型、范围、迁移和备份。
- CLI 预览模式不写文件。
- systemd unit 路径、用户会话和退出行为。
- 日志中的地址、文本、音频路径和个人路径泄漏检查。

## 3. 真机验收矩阵

| 场景 | 必测内容 |
|---|---|
| 首次配对 | pair/trust/connect、ATVV 能力、全部输入节点 |
| 单次语音 | 原生界面、非静音、首字、尾字、一次上屏 |
| 连续语音 | 同一 BLE 连接至少五段，无状态污染和重复 CAPS |
| 长句 | 缓冲、保持连接、尾部完整、无可复现截断 |
| 断线 | 关闭/唤醒遥控器、F9 释放、自动重连 |
| 桌面 | 编辑器、浏览器、终端，不自动提交、不误切焦点 |
| 全键 | 13 键 down/up、默认动作、物理键盘隔离 |
| 服务 | 登录启动、注销清理、异常重启、升级与卸载 |

每个失败都要区分：BlueZ、ATVV、解码、PipeWire、Superwhisper 输入选择、F9、识别或上屏。不能用“识别不准”掩盖音频链路问题。

## 4. 里程碑

### M0：设计与探针准备

- 设计文档、仓库和参考实现边界完成。
- Rust 工具链可用，空项目可构建。
- `scan`/`doctor` 只读输出框架完成。

完成定义：能在 Redmi 构建并运行只读诊断，不修改系统。

### M1：RC003 硬件探针

- 完成配对、ATVV 能力读取和 13 键 raw 事件记录。
- 确认 RC003-MC 的帧长、语音键码、单键 rollover 和多节点形态。

完成定义：获得目标设备的真实能力基线，文档中的待验证假设被替换为事实。

### M2：实时音频桥

- 完成 ATVV 会话、ADPCM、PipeWire 源和重连。
- Superwhisper 能枚举并选择虚拟麦。

完成定义：五段真实音频和断线恢复通过，系统默认音频未改变。

### M3：原生 Superwhisper 听写

- 完成语音键 F9 down/up、输入权限和退出清理。
- 完成首字/尾字测量，只在有证据时加入补偿。

完成定义：编辑器、浏览器和终端中的原生实时界面、识别和一次上屏全部通过。

### M4：完整按键

- 完成其余 12 键的安全默认动作。
- 按需加入 tap/hold/double、层和前台应用 profile。

完成定义：13 键逐项验收，普通键盘和其他设备不受影响。

### M5：常驻与发布候选

- 完成用户服务、日志、配置迁移、升级和卸载。
- 构建发布资产并验证依赖、许可和校验值。

完成定义：重新登录后可自动恢复，卸载后无残留进程、卡键或虚拟设备。

## 5. 最终完成定义

项目只有同时满足以下条件才算第一版完成：

1. RC003-MC 的语音和 13 键都有真机证据。
2. Superwhisper 原生界面可见，音频与 F9 生命周期一致。
3. 首字、尾字、连续会话、断连和重启均通过。
4. 不修改系统默认音频和物理键盘行为。
5. 默认无录音文件、无文本日志、无云端上传。
6. 安装、卸载和故障恢复均可重复执行。

## 6. 实施中仍需决策的事项

- 新仓库的公开许可证和需要复用的具体 MIT 代码范围。
- 首字/尾字是否需要 pre-roll 或 release drain，以及实测值。
- RC003-MC 的电源键和厂商键安全默认动作。
- 前台应用 profile 的最小集合；第一版不追求复制所有上游配置。
- 是否需要长期保留高级层和宏，还是保持小巧的 13 键直接映射。

## 7. Review 回归覆盖

针对代码审查发现的问题，已补充或更新以下回归验证：

- 松开延迟只影响 up 事件，并在没有后续原始输入时也能到期释放。
- 126 字节带头帧拆成多个通知后仍能正确重置并解码。
- `mapping.enabled = false` 只禁用普通按键映射，语音键继续工作。
- 非项目生成的 unit 在停止或卸载前就会被拒绝。
- unit 中的 `$`、反引号和 `%` 不再被 shell 或 systemd 展开。
- 服务可执行文件安装到 `~/.local/bin`，不依赖 `target/`。

## 8. 第二轮 review 回归覆盖

- `120 + 6` 的带头帧拆包不会在首个 120 字节处误判为无头帧。
- `AUDIO_START` 清空音频帧格式、分包缓存和解码器状态。
- 非 16 kHz 协商被拒绝，新会话开始前清理 PipeWire 残留 PCM。
- unit 中的单引号、`$`（写成 `$$`）、反引号和 `%` 均有对应处理。
- 覆盖安装通过 `try-restart` 启动新二进制。
- 非 RC003 节点缓存按 `(路径, dev/inode)` 识别，兼容 eventN 复用。

## 9. 第三轮 review 回归覆盖

- PCM 帧携带会话编号，旧会话帧被丢弃，新会话首帧不被 Reset 误删。
- Reset 状态改用 `Arc<AtomicU64>` 跨线程共享，不再使用 `Rc<Cell>`。
- SIGINT/SIGTERM 只在启动时注册一次，退出时仍执行完整清理。
- unit 路径使用白名单字符，避免 systemd 与 shell 双层转义。
- 覆盖安装只执行一次 `restart`。
- `stat` 失败且缓存为空时不会误跳过输入节点。

## 10. 第四轮 review 回归覆盖

- 注入只返回 `WouldBlock` 的事件源，验证到期后仍会发出松键事件。
- 释放窗口内重按会取消待发 up，随后可以形成新的完整会话。
- 成功会话后重置重连退避，失败重试前执行等待。
- 只按帧编号判定新旧会话，消除读取当前编号与取帧之间的竞态。
- PCM 通道容量限制为 32，满或关闭时丢弃新帧，不再无限堆积。

## 11. 第五轮 review 回归覆盖

- PCM 通道满时先丢弃最旧帧，再写入新帧，保持实时音频连续。
- 每次 `AUDIO_START` 都递增会话编号，连续两次开始也有测试验证，避免上一句尾音混入下一句。
- 帧编号只用于判断会话新旧，不再依赖读取当前编号与取帧之间的时序。

## 12. 第六轮 review 回归覆盖

- `ArrayQueue::force_push` 在容量满时覆盖最旧帧，新增测试验证保留最新音频。
- 会话编号在 `run_session` 收到帧时写入 `AudioFrame`，避免旧帧被错误标成新会话。
- `frame_is_newer` 的文档注释已移到正确函数。

## 13. 第七轮 review 回归覆盖

- 无法判定帧格式的分片保留在缓冲中；`80 + 80 + 80` 字节通知可解出两个 120 字节裸帧。
- 只把底层 `bluer::ErrorKind::NotPermitted` 判为设备占用；重新选中设备时清除排除标记。CTL、RX 和设备事件流的 EOF 均退出当前会话以便重连，订阅连接事件后再次检查当前状态。
- PipeWire 创建音源前完成启动握手，启动失败或运行中错误会传回主循环并使服务失败退出。
- `AUDIO_START` 先推进会话编号再请求 PipeWire 复位；复位只清理更旧会话的 PCM，不误删已到达的新句开头。按需录音用 watch 保存最新消费者数，新会话发现已有消费者时立即发送 MIC_OPEN。
- `--device` 同时约束 BLE 和 evdev，输入节点通过 `unique_name()` 匹配选定的蓝牙地址。
- 显式配置路径不存在时拒绝启动；卸载仅处理本项目生成的 unit，停用失败时保留文件并返回错误。
- udev 规则更名为 `70-rc003-omarchy.rules`，先于 systemd 的 `73-seat-late.rules` 添加 `uaccess`。

## 14. 第七轮真机回归

2026-11-19 在 Redmi 对新构建和安装后的 `~/.local/bin/rc003-omarchy` 完成以下检查：

- `doctor` 的 bluetooth、uinput、input、PipeWire 和 Superwhisper 五项全部通过，服务为 enabled/active。
- 停止常驻服务后，显式 `uinput_end_to_end_voice_mapping` 集成测试通过；随后恢复服务。
- SIGTERM 停止时日志确认 MIC_CLOSE、PipeWire 和 input manager 均完成清理。
- 主动断开蓝牙后服务保持运行，自动重连并在首次 CAPS 超时后重试成功。
- 使用 `--device` 前台运行时正确接管目标 evdev 节点；第二实例因真实 BlueZ NotPermitted 被拒绝，原服务不受影响。
- 注入不存在的 PipeWire remote，启动以状态 1 失败退出，不再留下无麦克风的假运行进程。
- `pw-cat` 持续连接并启用 `--mic-on-demand` 时，蓝牙重连后收到新的 AUDIO_START，验证已有消费者路径。

## 15. 第八轮 review 回归覆盖

- PipeWire 实时回调独占 PCM 缓冲，跨线程只发布原子会话编号；启动等待 Paused/Streaming，Error 和意外 Unconnected 会传播给守护进程。
- v1.0 不再按通知长度猜测帧格式。默认按 CAPS 帧长解析裸数据，`120 + 120` 与 `126 + 114` 得到相同 PCM，`1 + 100×120` 不再停滞；`audio.frame_layout = "headered"` 保留显式 126 字节带头帧支持，旧配置默认 `raw`。
- 锁定设备排除在 5 秒后过期，发现过程受排除期限约束，只有一只遥控器时也能自动重试。
- BlueZ 通知 socket 的零字节读取按 EOF 结束流，覆盖 SOCK_SEQPACKET 对端关闭。
- 默认配置不存在时 unit 省略 `--config`；自定义配置路径保留在生成的 unit 中。
- 默认运行日志只输出脱敏蓝牙地址。

2026-11-19 在 Redmi 重新安装新二进制后，140 个单元测试、显式 uinput 集成测试、Clippy、release build 和五项 `doctor` 检查通过。缺失默认配置与自定义配置的 unit 预览均通过；持续 `pw-cat` 消费者在真实蓝牙重连后再次收到 AUDIO_START。自动发现模式下，第二实例首次遇到真实占用，另一实例退出 5 秒后自动找回同一遥控器并完成协商。默认日志已实测脱敏，服务恢复为 enabled/active。本轮未重新生成 Superwhisper 语音记录。

## 16. 第九轮 review 回归覆盖

- 正常 AUDIO_STOP 保持 generation 以排空尾音；任何连接退出或会话取消都通过退出守卫使排队旧 PCM 失效。
- PipeWire 故障监听覆盖协商、录音、退避和蓝牙重连，录音故障与正常停止共用 MIC_CLOSE 清理路径，最终返回失败让 systemd 重启。
- 故障通知使用 watch 保存最新状态。即使通知已被 select 消费或通道异常关闭，后续等待和最终退出仍能失败关闭，不静默丢错。
- 自定义相对配置路径在生成 unit 前转为绝对路径，避免工作目录变化后找不到配置。

当前验证：rustfmt 通过；123 项可移植测试在 macOS 全部通过，包括实际 ShutdownSignal 代码的通知消费与通道关闭测试。项目完整 143 项单元测试、Clippy、release build、uinput 集成测试、PipeWire 真机故障注入及最终服务恢复尚未执行，原因是 Redmi 合盖离线。待设备恢复后必须完成这些验收，再更新本节为真机通过。

## 17. 第十轮 review 回归覆盖

- 主动销毁旧 PipeWire 音源前先保存真实故障并注销 watch；正常通道关闭不再触发错误退出，所有提前退出统一检查故障并返回正确状态。
- 正常 AUDIO_STOP 的尾音最多保留一秒；最后一个消费者离开时立即使剩余尾音失效，下一位客户端不会读到历史音频。
- 发现、首次特征解析、重连特征解析及厂商握手的嵌套等待全部与故障通知竞争，不再被 sleep 屏蔽。

当前验证：rustfmt 和差异检查通过；124 项可移植测试在 macOS 全部通过。项目完整 144 项单元测试、Clippy、release build 和 Redmi 真机复验仍待中午设备恢复后执行。本轮修复尚未安装到 Redmi。

## 18. 第十一轮 review 回归覆盖

- 默认和按需模式都始终监听 PipeWire 消费者，输出侧在每次真实 `1→0` 时推进 generation，覆盖 watch 合并的快速 `1→0→1`。
- 新增共享 `TailDeadline`，以单调时钟和原子 compare-exchange 管理尾音期限；输出回调独立使过期 PCM 失效，不等待 BLE 写入或 ATVV 定时器。

Redmi 验证：148 个单元测试及显式 uinput 集成测试通过，Clippy、release build 和五项 `doctor` 通过。默认模式实测消费者计数 `1→0→1`；蓝牙离线期间重启 PipeWire 会以状态 1 退出并由 systemd 换新 PID，恢复后五项检查通过。第二实例正常销毁旧音源后继续发现，并在占用解除 5 秒后自动完成协商。新二进制已安装到 `~/.local/bin/rc003-omarchy`，服务恢复为 enabled/active。随后通过实体语音键完成一次真实 HoldToTalk 听写，日志确认完整 AUDIO_START/AUDIO_STOP，Superwhisper 新增一条来自 `RC003 Remote Microphone` 的识别记录并成功上屏。

## 19. 第十二轮 review 回归覆盖

- `AUDIO_STOP` 在活跃录音状态先设置一次尾音期限并切到 Connected，再等待可能阻塞的 MIC_CLOSE 写入；迟到的停止确认不重新设期限。
- 输出端先读取当前 generation，过期后用 compare-exchange 只推进仍为旧值的 generation；若新录音已开始，旧过期路径不能再次推进代数。

Redmi 验证：149 个单元测试、Clippy 和 release build 通过，新二进制已安装，五项 `doctor` 全部通过。生产改动仅涉及停止顺序与 generation 原子推进；本轮真实听写待确认。

## 20. 第十三轮 review 回归覆盖

- 用 `stream_generation` 标记录音及其首次停止；只有新 AUDIO_START 才重置停止标记，Opening 状态的迟到确认不会给上一代续期。所有本地关闭路径都在 BLE 写入前设置期限。
- 每个音频帧使用当前输出段的 generation，不再在出队时盲目读取可能已变化的全局代数。
- 首次厂商握手与 SIGTERM/故障通知竞争，首次 GATT 等待不再阻塞退出。
- 重复停止测试加入真实 3ms 间隔，避免 Tokio 虚拟时钟与 std::time::Instant 不同步造成假通过。

Redmi 验证：149 个单元测试、Clippy、release build 和五项 `doctor` 通过，新二进制已安装并运行。生产差异仅 `atvv.rs` 与 `main.rs` 两处文件；本轮真实听写待确认。

## 21. 第十四轮 review 回归覆盖

- generation 切换时只在协议存在跨代残帧的情况下丢弃首个完整帧；无残帧的新完整帧立即可输出，后续完整帧也恢复输出，不再持续静音。
- 只忽略与实际待确认关闭请求原因匹配的 AUDIO_STOP，保留 Opening，其他停止原因仍按当前状态正常处理。
- 回归测试改用真实 v1.0 分片协议，覆盖无残帧切换、跨代残帧、后续恢复、Opening 再次关闭和真实 3ms 期限断言。

当前验证：126 项可移植测试全部通过，rustfmt 和差异检查通过。Redmi 当前无法解析地址，完整 149 项单元测试、Clippy、release build 和真机复验尚待执行。

## 22. 第十五轮 review 回归覆盖

- RX 在底层读取任务收到通知时附上 generation，会话晚处理旧包也不会把它标给新客户端；读取任务使用有界队列，Drop 时中止并释放 GATT socket。
- 本地 MIC_CLOSE 记录待确认原因，只有原因匹配时才在重新打开期间忽略确认；NotificationsDisabled 不再被误吞。
- AUDIO_SYNC 清空协议残帧时同时清除 drop_next_frame，后续完整帧不再误丢。
- 模拟测试覆盖 4 帧旧包延迟处理、SYNC 后 42/43 连续帧、两种停止原因、再次关闭和读取任务取消。

当前验证：126 项可移植测试全部通过，rustfmt 和差异检查通过。Redmi 仍无法解析地址，完整 150 项单元测试、Clippy、release build 和真机复验尚待执行。

## 23. 第十六轮 review 回归覆盖

- `anyhow::Context` 改为 `Context as _`，消除与 `std::task::Context` 的编译冲突。
- RX 队列满时不再等待 `send().await`：尝试弹出最旧包并写入新包，锁竞争时也继续读取，避免旧包积压后被重新标代。
- `AUDIO_START` 后允许已提前读取的新句首批包归入新代，普通旧包仍丢弃且 generation 不回退，避免新录音开头静音。
- 队列策略测试验证容量为 1 时旧包被弹出、最新包保留；首包测试显式使用提前到达的旧代标记。

当前验证：126 项可移植测试全部通过，rustfmt 和差异检查通过。Redmi 仍无法解析地址，完整 151 项单元测试、Clippy、release build 和真机复验尚待执行。

## 24. 第十七轮 review 回归覆盖

- 修复 Context 重名、Pin 轮询、锁守卫、泛型括号和构造函数生命周期；`tokio-stream` 从 dev-dependencies 移到正常 dependencies，生产构建不再漏依赖。
- 单一通知协调器按 CTL 优先读取，`AUDIO_START` 在底层先推进 generation，再为 RX 标代；会话不二次递增，也不用“第一个旧包”猜测边界，避免旧尾包串音和新首包截断。
- RX 使用固定 64 槽 `ArrayQueue::force_push` 和关闭标记，满时覆盖最旧包，Drop 中止任务，EOF 正常结束。
- 饱和测试明确验证 64 个旧包后再来 5 个新包时保留 59 个旧包和最后 5 个新包，防止把“覆盖最旧”误写成“全部更新”。

Redmi 验证：153 个单元测试及显式 uinput 集成测试通过，Clippy 和 release build 通过，新二进制已安装，五项 `doctor` 全部通过。覆盖首句顺序、旧尾包、跨代残帧、69 包饱和、停止原因、SYNC 和任务取消。真实 HoldToTalk 听写形成新的 Superwhisper 识别并成功上屏；主动断开后自动重连并重新完成能力协商。

## 25. 第十八轮 review 回归覆盖

- CTL 与 RX 合并为 `SessionNotification` 有序流，会话按同一顺序处理开始事件和音频，不再让首帧越过 AUDIO_START。
- 仅在流式录音仍活跃时于开始事件前用旧 generation 非阻塞排空积压 RX，再递增编号并发送开始事件，避免旧尾音跨代；v0.4 的一字节开始同样推进编号。
- 队列满时只淘汰最旧音频，控制事件不被丢弃；协调器关闭返回 EOF，Drop 中止任务并释放订阅。
- 覆盖积压旧包顺序、首帧、v0.4、69 包饱和、控制事件保留、SYNC、统一 EOF 和读取任务取消。

Redmi 验证：156 个单元测试及显式 uinput 集成测试通过，Clippy 和 release build 通过，新二进制已安装，五项 `doctor` 全部通过。真实 HoldToTalk 听写形成新的 Superwhisper 识别并成功上屏；主动断开后自动重连并重新完成能力协商。

## 26. 第十九轮 review 回归覆盖

- 改用 BlueZ 单一 D-Bus `PropertiesChanged` 消息流确定控制与音频顺序，避免用流式布尔值和 socket 优先级猜测边界。
- `ControlPacket` 在入队时携带 generation，START 与后续音频使用同一编号；控制事件在音频饱和时仍保留，协调器关闭返回 EOF 并释放订阅。
- 测试直接断言开始、停止、下一次开始和断连时的流式状态，并分别验证 START 前旧包与 STOP 前尾包的事件顺序。

Redmi 验证：157 个单元测试及显式 uinput 集成测试通过，Clippy 和 release build 通过，新二进制已安装，五项 `doctor` 全部通过。真实 HoldToTalk 听写形成新的 Superwhisper 识别并成功上屏；主动断开后自动重连并重新完成能力协商。

## 27. 第二十轮 review 回归覆盖

- 用单一 D-Bus `PropertiesChanged` 消息流替代两路 `AcquireNotify` socket，直接采用 BlueZ 的信号顺序，不再根据状态动态选择读取优先级。
- 使用 zbus Proxy 分别开启 CTL/RX StartNotify，路径由 adapter、device、service 和 characteristic ID 构造；消息按路径分类并解码 `Value`。
- START 使用 `ControlPacket { data, generation }`，与后续音频共享编号，避免两次开始积压时读取到下一代编号。
- 反例矩阵包括首帧、尾帧、快速短按、关麦立即重录、相同内容不同代、69 包饱和、控制事件保留、v0.4、SYNC 和统一 EOF。

Redmi 验证：157 个单元测试、Clippy 和 release build 通过，真实 StartNotify、能力协商和五项 `doctor` 通过。真实 HoldToTalk 听写形成新的 Superwhisper 识别并成功上屏；主动断开后自动重连并重新完成能力协商。连续短按与立即重录仍作为后续专项验收场景。

## 28. 第二十一轮 review 回归覆盖

- 每次订阅前查询 `org.bluez` 当前唯一总线名，同时用于 `MatchRule::sender` 和 StartNotify 目标，忽略同路径的其他发送者。
- `StartNotify` 的 `org.bluez.Error.NotPermitted` 映射为 `bluer::ErrorKind::NotPermitted`，复用指定设备报错和自动发现暂时跳过逻辑。
- 用“适配器＋设备地址”的 Linux 抽象 Unix socket 防止本程序多实例同时写设备；句柄覆盖握手、录音和重连，进程退出自动释放。

Redmi 验证：158 个单元测试及显式 uinput 集成测试通过，Clippy 和 release build 通过，五项 `doctor` 通过。指定设备的第二实例以状态 1 被拒绝，原服务保持 active；自动发现模式在锁释放 5 秒后找回同一设备并完成协商。

## 29. 第二十二轮：常驻音源、零唤醒事件驱动输入与运维诊断闭环

- PipeWire 虚拟麦克风源提升到 `'discover` 循环之前创建并常驻整个进程生命周期，即使开机遥控器休眠或跨设备重新发现期间也保持源存在，避免 Superwhisper 回退；正常退出前注销故障 watch 并释放源，真实故障仍返回非零退出码。
- 输入线程改用 `poll()` + `pipe2` 退出管道 + `/dev/input` inotify（`IN_CREATE | IN_ATTRIB | IN_MOVED_TO | IN_DELETE`），正常空载唤醒降至 0 次/秒（`timeout = -1`），udev 授权 `IN_ATTRIB` 即刻触发发现；`probe_keys` 同步改用 `poll()`。
- `VirtualDevice` 提升为输入线程常驻复用，避免遥控器休眠断连时反复增删输入设备及丢松键风险；按键事件逐个发送 `SYN_REPORT`；`/dev/uinput` 或经 sysfs 识别的 RC003 节点权限/独占失败记录一次性 `warn` 告警，降级期间不独占设备。
- 配置新增 `[audio]` 的 `frame_timeout_secs`、`keep_alive_secs`、`mic_on_demand` 及 `[mapping.keys]` 非语音键自定义覆盖（向后兼容旧配置），`Config::validate()` 新增蓝牙 MAC 格式与 `audio.node_name` 校验；`doctor` 按实际配置检查自定义源名并新增 Superwhisper hold 快捷键（含 `shortcuts.hold` 与 `system.hold` 是否已 apply）一致性检查。
- D-Bus 接口与对象路径统一为 `org.rc003omarchy.Daemon` / `/org/rc003omarchy/Daemon`；修正 systemd 示例路径至 `%h/.local/bin/rc003-omarchy`；补充 `THIRD_PARTY_NOTICES.md` 与 `.github/workflows/ci.yml`。

Redmi 实机验证：176 个单元测试通过、显式 `uinput_end_to_end_voice_mapping` 集成测试通过（实测 udev `IN_ATTRIB` 233ms 即刻唤醒接管、同批 down/up 逐事件发出）、Clippy（默认与 `--features dbus`，`-D warnings`）及 release build 通过，六项 `doctor` 检查（`bluetooth`、`uinput`、`input`、`pipewire`、`superwhisper`、`shortcut`）运行正常。

## 30. 第二十三轮：唤醒瞬时接管、锁语义收口与边界修复

- 固定设备模式改为在启动常驻资源之前即获取每设备抽象 Unix socket 锁，重复实例在创建 PipeWire/uinput/D-Bus 资源前快速拒绝；已持锁后若 `StartNotify` 遇 BlueZ `NotPermitted`，固定设备模式降级为告警并退避重试（避免链路加密/重连竞态导致守护进程退出），自动发现模式保持临时排除并重找设备。
- `InputManagerHandle` 提升到 `'discover` 循环之前与 PipeWire 音源一同常驻，支持通过控制管道热更新目标蓝牙地址（`TargetAddress`）：固定设备模式开机即监听并瞬时接管唤醒后的 RC003 节点，自动发现模式以 `TargetAddress::None` 常驻虚拟设备，拿到设备锁后再动态绑定目标并在释放时清理旧设备与挂起按键。
- D-Bus 服务（`--features dbus`）同步提升至外层常驻，`DaemonInfo.device_address` 改由 `watch::Receiver<String>` 动态反映当前目标设备，断连或切回自动发现时不反复注销重绑总线名；进入已连接状态前排空断连期间积压的外部控制命令。
- BLE 音频通知队列满载淘汰最旧音频包时在下一包标记 `discontinuity`，会话与 v1.0 解码器据此清空残留半帧（`headered` 模式下若后续包未帧对齐则等待下一次 `AUDIO_SYNC`），防止 64 包溢出后永久帧偏移。
- PipeWire 消费者监控在 `Stream::state_changed` 报告自身 `node_id` 之前到达的 `Link` 全局对象先暂存并绑定 `info` 监听器，拿到 `node_id` 后立即提升匹配链路并释放无关代理，消除启动竞态丢链接。
- 按键重复改为按映射后的目标键（`Up/Down/Left/Right/Backspace/VolumeUp/VolumeDown/PageUp/PageDown`）判定而非原始键码；`doctor` 与 `test-keys` 的 evdev 枚举同步按 `--device` / `config.device.address` 过滤；`udev/70-rc003-omarchy.rules` 第一条显式补充 `GROUP="input", MODE="0660"`。

Redmi 实机验证与 CI：GitHub Actions CI（`#45279841839`）与 Redmi 真机同步通过——`cargo fmt --all -- --check`、182 个单元测试（`182 passed, 0 failed, 1 ignored`）、显式 `uinput_end_to_end_voice_mapping` 集成测试（经 udev `IN_ATTRIB` 295ms 唤醒接管）、Clippy（默认与 `--features dbus`，`-D warnings`）及 release 编译全部通过。新二进制已通过 `service install --apply --force` 安装至 `~/.local/bin/rc003-omarchy`，更新后的 `udev/70-rc003-omarchy.rules` 已安装至 `/etc/udev/rules.d/` 并重载触发，systemd 用户服务为 `active/enabled`，在遥控器休眠待连状态下 PipeWire 源与输入线程均已常驻运行，`doctor` 的 `bluetooth`、`uinput`、`pipewire`、`superwhisper`、`shortcut` 五项通过，`input` 因遥控器当前休眠报 `warn`（唤醒后即刻接管）。

## 31. 第二十四轮：三项边角清零（设备移除退回、全局 `--adapter` 对齐与跨代溢出边界）

- `ensure_connected`、首次与重连 `resolve_chars` 以及 `negotiate` 统一识别 BlueZ `DoesNotExist` / `UnknownObject` 或适配器设备列表缺失，运行中执行 `bluetoothctl remove` 时立刻退回外层 `'discover` 重新发现（自动发现模式同步重置 `TargetAddress::None` 与 D-Bus `device_address`），不再对失效对象持续轮询告警。
- 移除 `Command::Scan` 上与全局 `Cli::adapter` 重名的局部字段，统一将 `--adapter` 透传至 `scan` 与 `doctor`（`scan` 同时支持显式 `--device` 过滤），并补充 `Cli::command().debug_assert()` 与前置/后置参数解析单测。
- `NotificationQueue` 满 64 包淘汰音频包时仅在下一音频包与被淘汰包属于同一 `generation` 时标记 `discontinuity = true`，避免跨 `AUDIO_START` 边界淘汰旧句音频误伤新句首包（尤其 `headered` 分包模式）。

Redmi 实机验证：`cargo fmt --all -- --check`、185 个单元测试（`185 passed, 0 failed, 1 ignored`）、Clippy（默认与 `--features dbus`，`-D warnings`）及 release 编译全部通过，已重新安装 `~/.local/bin/rc003-omarchy` 并重启服务验证通过。
