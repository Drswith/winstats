# 内置后台采集验收 — 2026-10-22

分支：`codex/extensible-metrics`。本轮完成内置采集线程隔离和 9 个通用绑定；保留前一轮未提交改动，没有提交或发布。

## 实现范围

- `src/metrics/local.rs`：UI 只持有快照和期望设置。硬件初始化、常规采集、详情查询、进程筛选、适配器选择和配置重建在后台执行。
- 输入、输出各保留最新一份完整状态。设置合并不会丢失最后一次选择；快照队列不积压。创建、轮询、重载及销毁不等待硬件查询完成。
- 重载复用仍在运行的线程，用配置代次拒绝旧结果；已退出的线程可通过重载重建。关闭时通知停止，避免 UI join 卡在驱动调用上。
- `src/metrics/builtin.rs`：CPU、内存使用量/比例、网络收/发、磁盘读/写、GPU、电量，共 9 个绑定。序号、采样时间和有界历史由后台维护；UI 重绘不会延长 TTL。
- 专用 Widget 与通用 Widget 消费同一份数据。外部提供器的 Registry 项不被本地快照覆盖。Sharing 只发布实际收到的新快照。
- Release 改用 Rust panic unwinding，让采集线程 panic 后可标记 offline，而不是直接 abort 整个进程。原生崩溃仍不受线程隔离保护。
- `--diag` 是独立非 UI 命令，继续使用同步采集。

指标目录、配置和限制见 [providers.md](../providers.md)。

## 自动验证

工具链 Rust 1.77.0，Windows x64 MSVC。

| 检查 | 结果 |
| --- | --- |
| `cargo +1.77.0 test --workspace --locked` | 49 通过，0 失败，2 忽略（真实硬件验收和既有联网更新检查）。 |
| `cargo +1.77.0 test -p winstats --locked real_windows_snapshot_details_and_reload -- --ignored --nocapture` | 真实 Windows 采集、进程详情、配置重载、内存绑定一致性及线程停止通过。 |
| `cargo +1.77.0 build --workspace --release --locked` | 通过；界面验收使用 release 构建。 |
| `cargo +1.77.0 clippy -p winstats --all-targets --locked` | 成功；新增 `local.rs` / `builtin.rs` 无诊断，其余原有代码仍有 15 项警告（测试目标含重复项）。不宣称全项目 `-D warnings` 通过。 |
| `git diff --check` | 通过。 |

新增测试覆盖：阻塞采集期间 1000 次 UI 轮询/设置更新、保留最终设置、阻塞期间退出不等待、生产者多次发布后只消费最新快照、拒绝旧配置代次、外部指标保留、重复轮询不刷新时间/历史、TTL 过期、panic 后离线且保留最后值、9 项映射以及错误和恢复。

首次沙箱内 HTTP 测试因回环套接字访问被拒绝失败；通过正规权限流程在沙箱外重跑通过。Release 首次沙箱构建遇到归档写权限限制，沙箱外构建成功。未将这些失败计为通过。

## 界面验收

实测 Windows 11 IoT Enterprise LTSC，10.0.26100，x64；2560 × 1440 桌面、150% 缩放、横向任务栏。交互使用 Computer Use `@oai/sky`，证据按用户要求保存全屏图。

隔离配置位于 `tmp/local-collector-20261022/appdata/winstats/config.toml`。截图含其他桌面窗口，保留在 Git 忽略的 `tmp` 中，不作为发布资源。

| 项目 | 证据与结论 |
| --- | --- |
| 通用指标详情 | CPU 显示 `builtin/cpu/usage`、有效状态、online 数据源和历史曲线。[全屏](../../tmp/local-collector-20261022/evidence/02-cpu-generic.png) |
| 原有 CPU 弹窗 | 核心图、时钟、进程数和进程列表正常显示。[全屏](../../tmp/local-collector-20261022/evidence/03-cpu-details.png) |
| 进程筛选操作 | SYS 开关操作后隔离配置保存 `proc_show_system=false`。当前悬停列表按既有逻辑冻结；此截图只证明开关与持久化，不单独证明每条进程的分类。[全屏](../../tmp/local-collector-20261022/evidence/04-process-filter.png) |
| 菜单重载 | 选中“重新加载配置”后执行，日志出现 `config reloaded`，采集和显示继续。[选中](../../tmp/local-collector-20261022/evidence/06-reload-selected.png)、[重载后](../../tmp/local-collector-20261022/evidence/07-reloaded.png) |
| 实际嵌入模式 | 不带 `--ui-test` 启动，日志 `floating=false`。4 控件布局中，CPU 19% / 19.1%、内存 50% / 50.0%，显示精度不同，底层样本一致。[全屏](../../tmp/local-collector-20261022/evidence/09-embedded-compact.png) |
| 宽布局限制 | 10 个数值控件与任务栏应用图标重叠；保留宽配置 `wide-config.toml`，不将该布局计为显示通过。本轮未修改任务栏空间分配逻辑。[全屏](../../tmp/local-collector-20261022/evidence/08-embedded.png) |

验收日志记录过一次 `slow tick: 367 ms`。本轮保证硬件采集不在 UI 线程执行，不保证窗口绘制、图标加载等所有 UI 工作均小于某个时限；阻塞注入验证的是 UI 侧采集接口的非阻塞性。

## 清理与边界

- 已停止隔离验收进程，恢复验收前的 `Downloads/winstats.exe`，并核实只有该原程序在运行。
- 原用户配置 SHA-256 前后一致：`ECB18129F7E45D4D30CA790B923DCB0899FAA26CCD4C29B05FC8C2B1A6BAAE59`。
- 界面验收二进制 SHA-256：`869350BE0D29FB64418BDEDFC27593D3B971AFA28868BBEFDD6EB5C86892127C`。
- 测试输出、配置、截图和清理记录在 `tmp/local-collector-20261022/`；保留用于本机复核。
- 所有内置采集目前共用一条后台线程。慢查询会延迟其他内置指标；外部提供器和 UI 仍独立运行。永久阻塞的原生调用不能取消，重载不会无限增生线程。
- GPU/电池等不可用来源会产生通用错误样本；底层每个 Windows API 的独立失败诊断没有全面改造，专用 Widget 继续保留旧值语义。
- 本轮未验证多屏、多 DPI、竖向任务栏、浅色主题、Windows 7/10、ARM64/x86；远程自定义指标转发尚未实现。
