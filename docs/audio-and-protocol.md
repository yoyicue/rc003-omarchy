# ATVV 与音频桥设计

## 目标设备实测基线

2026-11-18 在 Redmi 上已完成配对、bonded、trusted 和系统连接。GET_CAPS 返回 `0b 01 00 02 03 00 78 00 00`，确认协议 `0x0100`、16 kHz codec mask `0x02`、帧长 120 字节。

后续已收到真实 AUDIO_START/AUDIO_STOP，并由 Superwhisper 完成三条真实语音的识别与上屏。音频链路和真实语音路径已经验证。

## 1. 配对与设备身份

首次配对由用户显式完成：

1. 同时长按 RC003 的“主页 + 菜单”进入配对广播。
2. 使用 `bluetoothctl` 或 Omarchy 蓝牙界面执行 pair、trust、connect。
3. 用方向键唤醒遥控器，再运行 `rc003-omarchy scan` 和 `doctor`。
4. 用户确认设备后再将地址写入本地配置；自动发现只作为便利，不作为身份认证。

守护进程不得自动配对、清除系统 bond 或断开其他蓝牙设备。

## 2. ATVV 通道

参考实现使用以下 UUID：

| 用途 | UUID |
|---|---|
| Service | `AB5E0001-5A21-4F05-BC7D-AF01F617B664` |
| Transmit | `AB5E0002-5A21-4F05-BC7D-AF01F617B664` |
| Audio | `AB5E0003-5A21-4F05-BC7D-AF01F617B664` |
| Control | `AB5E0004-5A21-4F05-BC7D-AF01F617B664` |

实现不得仅凭名称认定 RC003。候选设备需要结合已配对身份、设备信息和 ATVV 服务确认，并允许用户固定地址消除同名设备歧义。

## 3. 会话握手

正常顺序：

1. 连接 BLE 并发现 ATVV service 及三个 characteristic。
2. 订阅 Audio 和 Control notify。
3. 发送 GET_CAPS，设置有限超时。
4. 解析 CAPS_RESP，记录协议版本、codec、采样率和帧长。
5. 等待遥控器 MIC_REQUEST。
6. 按协议版本回复 MIC_OPEN。
7. 等待 AUDIO_START 和必要的 SYNC，进入 streaming。
8. 收到 AUDIO_STOP 后发送对应 MIC_CLOSE，并完成尾部排空。
9. 回到 ready；重复会话不得重复创建 PipeWire 节点。

RC003 第一版只接受 16 kHz IMA/DVI ADPCM。其他格式先显式拒绝并输出能力信息，不静默按错误格式解码。

## 4. 帧与解码

解码器需要覆盖：

- 参考固件的 120 字节裸音频帧。
- 可能出现的 126 字节带同步头帧。
- 跨通知边界的帧累积、错误对齐后的重新同步。
- SYNC 中 predictor 和 step index 的解码状态重置。
- 同一音频流内跨包保持解码状态，新会话必须重置。
- 重复包、乱序、截断和无效长度的计数与安全处理。

PCM 后处理第一版保持简单：可选三抽头平滑、可调数字增益和削顶保护。默认不启用降噪、自动增益、人声分离或云端处理。

## 5. PipeWire 虚拟麦克风

守护进程创建一个真正的音频源：

| 属性 | 初始设计 |
|---|---|
| `media.class` | `Audio/Source` |
| `node.name` | `rc003-omarchy` |
| `node.description` | `RC003 Remote Microphone` |
| 格式 | 16 kHz、单声道、16-bit signed PCM |
| 生命周期 | 随守护进程常驻，BLE 断开时无有效音频 |
| 默认设备 | 不修改系统默认 source/sink |

源常驻是为了减少 Superwhisper 启动录音时的设备发现延迟。没有语音会话时不得伪造现场环境音；实现可以输出静音或暂停流，但不得把缓存中的上一段语音重放。

`--mic-on-demand` 已实现但不是默认行为。PipeWire 通过 watch 通道保留最新消费者数量；每个新 ATVV 会话开始时都会读取该值，因此录音客户端跨蓝牙重连保持连接时也能重新触发 MIC_OPEN。2026-11-19 已在 Redmi 用持续连接的 `pw-cat` 消费者验证，蓝牙恢复后收到了新的 AUDIO_START。是否改为默认仍需长期延迟和隐私测试。

## 6. 缓冲与背压

- 缓冲只吸收蓝牙和音频调度抖动，不追求大容量。
- 初始目标为数十到数百毫秒的有限缓冲，具体值通过首字、尾字和延迟测试确定。
- 音频回调只做格式转换、出队和必要的静音填充。
- 队列满时不能阻塞 BLE 通知线程，也不能无限堆积旧音频；丢弃策略和计数必须可诊断。
- 欠载时输出静音并累计 underrun，不重复上一帧制造伪语音。
- 新会话开始时清理属于旧 generation 的样本。

## 7. 首字、尾字与 F9 协同

物理语音键同时触发 ATVV 和 evdev。默认时序：

```text
press:   F9 down immediately + ATVV MIC_OPEN
speak:   PCM continuously enters PipeWire
release: F9 up + ATVV stop/drain
```

这是实现假设，不是已验证结论。真机验收必须测量：

- F9 down 到 Superwhisper 开始读取的时间。
- MIC_OPEN 到首个有效 PCM 的时间。
- 首个有效语音是否落在录音窗口之外。
- AUDIO_STOP 后是否仍有需要保留的尾部包。

若首字丢失，优先保持 PipeWire 源和音频路径常驻，再考虑极短 pre-roll。若尾字丢失，可增加有上限的 F9 release drain；初值只能来自实测，不能预设为固定“优化”。

## 8. 重连与异常

- 连接失败采用有上限的指数退避，用户主动运行 `scan` 可触发立即重试。
- 协商、写命令、通知或音频流超时只影响当前 generation。
- BLE 断开时结束当前会话、释放可能保持的 F9，并丢弃旧音频。
- 重连后重新协商能力，不复用上一连接的帧长或会话 ID。
- Superwhisper 未选择 RC003 源时，音频桥仍可运行，但 `doctor` 必须给出明确提示。

## 9. 音频验收门禁

只有以下步骤全部通过，才可声明音频桥完成：

1. `superwhisper devices` 能看到 `RC003 Remote Microphone`。
2. 选择该设备后，系统默认输入仍保持原值。
3. 连续五段真人语音均有非零且不失真的音频，无上一段重放。
4. 首字、尾字、长句和连续会话没有可复现截断。
5. 断开、休眠、重连后不需要重启 Superwhisper。
6. 停止守护进程后虚拟源消失，Superwhisper 可回退到用户选择的其他麦克风。

## 10. 参考实现的复用边界

ATVVoice 已下载到 `~/Work/tries/atvvoice`，其 `src/pw.rs` 是本项目原生 PipeWire 源的主要实现参考。需要保留的关键点包括独立 PipeWire OS 线程、`Audio/Source`、`Direction::Output` 和显式 MONO channel position。

不得把 ATVVoice 文档中其他遥控器的 8 kHz、134 字节帧、逐帧 DVI 状态重置或 MIC_OPEN 参数直接套到 RC003-MC。目标设备当前已确认的是 ATVV v1.0、16 kHz 和 120 字节帧；其命令和 ADPCM 行为以 RC003 参考实现及本机后续真实语音验收为准。

## 11. 跨会话虚拟源生命周期

`rc003-omarchy` 的 PipeWire 源在进程生命周期内常驻。BLE 重连只结束当前 ATVV 会话并替换 PCM 生产者，不销毁或重新创建虚拟源，避免 Superwhisper 因源 ID 变化而回退或要求重新选择。进程收到 SIGINT/SIGTERM 后才统一关闭源。每次 `AUDIO_START` 都先递增会话编号，再请求 PipeWire 立即淘汰旧会话 PCM，连续听写不会重放上一句尾音。复位携带新会话编号：如果新句 PCM 已先到达，则不会误删其开头；PipeWire 仍依据每帧编号接收当前会话并丢弃旧帧。PCM 使用容量 32 的 `crossbeam_queue::ArrayQueue`，通过 `force_push` 在队列满时覆盖最旧帧，保持实时音频连续且无需共享接收端或 Mutex。会话编号在 `run_session` 收到帧时写入 `AudioFrame`，不再由下游解码任务补写。协商出的采样率不是 16 kHz 时会拒绝运行。
