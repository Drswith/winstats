# 扩展指标本机验收 — 2026-10-20

分支：`codex/extensible-metrics`。结论：本机核心功能通过；下列未验证范围不计入通过项。

## 环境与方法

- Windows 11 IoT Enterprise LTSC，build 26100，x64；2560 × 1440，150% 缩放，横向任务栏，中文界面、深色主题。
- 隔离配置：`tmp/ui-acceptance-20261020/appdata/winstats/config.toml`。原用户配置在验收结束时核对 SHA-256。
- 交互通过 Computer Use 的 `@oai/sky`。普通工具窗口无法被选取，因此交互使用 `--ui-test`；最后另以不带该参数的 `mode="embedded"` 启动验证实际嵌入显示。
- 按用户要求，后续验收使用完整桌面截图。截图经过 DPI 感知后保存为 2560 × 1440 PNG；未用拼接图代替桌面。
- 全屏截图包含本机其他窗口，保留在 Git 忽略的 `tmp/ui-acceptance-20261020/evidence/`，不作为仓库发布资源。以下链接是本机证据。

## 结果

| 项目 | 结果及证据 |
| --- | --- |
| 异步 HTTP 请求 | 快接口延迟约 200–250 ms；慢接口延迟 3 秒、客户端 1 秒超时。快指标继续更新，慢指标显示错误。进程集成测试也覆盖并发超时。 |
| 数值、曲线、布尔状态 | 正常显示；503 时状态关闭、最后成功数值保留并带异常标记，曲线出现空缺。[故障全屏](../../tmp/ui-acceptance-20261020/evidence/desktop-failure.png) |
| HTTP 自动恢复 | 服务恢复 200 后，无需重启宿主，数值继续更新、状态开启、曲线恢复。[恢复全屏](../../tmp/ui-acceptance-20261020/evidence/desktop-recovered.png) |
| 详情与 TTL | 可打开正常、超时和过期详情；只发送一次的 OLD 值为 42，持续心跳不会使该值恢复新鲜。[正常详情](../../tmp/ui-acceptance-20261020/evidence/healthy.png)、[超时详情](../../tmp/ui-acceptance-20261020/evidence/timeout.png)、[过期详情](../../tmp/ui-acceptance-20261020/evidence/stale.png) |
| 右键菜单 | 自定义指标列表及数据源入口正常打开；窗口截图遗漏菜单，全屏确认布局正常。[自定义菜单](../../tmp/ui-acceptance-20261020/evidence/desktop-custom-menu.png) |
| 显示开关 | 通过菜单隐藏并恢复 CPU 曲线，同时保存配置。恢复的指标追加到队尾。[隐藏](../../tmp/ui-acceptance-20261020/evidence/desktop-widget-hidden.png)、[恢复](../../tmp/ui-acceptance-20261020/evidence/desktop-widget-restored.png) |
| 拖动排序 | OLD 从队尾拖到队首，界面和 `widgets` 配置顺序一致。 |
| 停用数据源 | 通过菜单停用 website，配置为 `enabled=false`，HTTP 提供器进程退出，其他提供器保留，指标显示离线标记。[停用全屏](../../tmp/ui-acceptance-20261020/evidence/desktop-provider-disabled.png) |
| 数据源恢复与配置持久化 | 配置重新启用 website 后重启宿主；提供器重新采集，OLD 仍在队首，CPU 仍在队尾。此项使用配置修改和进程重启，未单独验证菜单重新启用。 |
| 实际任务栏显示 | 最终调试构建在不带 `--ui-test` 的嵌入模式下显示正常，日志 `floating=false`、任务栏连接成功。[嵌入全屏](../../tmp/ui-acceptance-20261020/evidence/desktop-embedded.png) |

## 验收中修正

并行进程测试中重启测试曾在 8 秒截止时失败，单独执行通过。修正握手阶段错误套用较短心跳超时的问题；握手固定使用 5 秒，建立目录后使用配置的心跳超时。集成测试总等待预算调整为 20 秒，容纳并发 PowerShell 冷启动和重试。

同时修正退出处理顺序：先处理 stdout 已写入的数据，再在空闲读取时检查进程退出，避免线程调度延迟导致最后一条样本被跳过。重启测试提供器现在发送样本后立即退出，不再依赖 100 ms 睡眠。超时测试诊断增加指标历史。

## 自动验证

- `cargo +1.77.0 test --workspace --locked`：44 通过、0 失败、1 忽略（既有联网更新检查）。
- `cargo +1.77.0 clippy -p winstats-metrics --all-targets --locked -- -D warnings`：通过。
- `cargo +1.77.0 build --locked --bin winstats`：通过，最终嵌入截图来自该构建。
- `cargo +1.77.0 build --release --locked --bin winstats --target-dir target\provider-validation`：通过。发布构建编译通过，本轮最终截图来自调试构建。
- `git diff --check`：通过；Git 提示部分文件未来转换为 CRLF，不属于检查失败。

## 未验证与边界

- 浅色主题、多显示器、多 DPI 切换、竖向任务栏、Windows 7/10、ARM64/x86 未进行本轮实机验收。
- 菜单重新启用数据源、配置热重载、嵌入模式中的鼠标交互没有单独完整复测。交互通过项来自 `--ui-test` 模式；嵌入模式通过项限显示与采集。
- 自定义指标经远程 agent 转发尚未实现，不属于本轮已交付的本地提供器功能。
- 布尔视图当前使用“开启/关闭”；异常使用灰色与 `!`，具体错误在详情中。这是当前行为，不表示已实现告警系统。

原用户配置 SHA-256 与验收前一致。测试宿主、本地 HTTP 服务及其提供器进程已退出；隔离配置和证据保留用于复现。全部代码改动仍在工作区，未提交。
