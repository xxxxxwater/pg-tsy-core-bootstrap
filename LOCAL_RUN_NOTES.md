# 本地运行记录 / 框架评估（pg-tsy-core-bootstrap）

> 本文档记录本地实机运行与随后的一轮修复/升级。运行时间：2026-09-14。
> 基线提交：ba30693（该提交在上游 CI 是失败的，见下）。

## 1. 快速开始

```powershell
cd C:\Users\Administrator\Desktop\pg-tsy-core
Copy-Item .env.example .env      # 影子模式不需要任何交易所密钥
docker compose up -d
docker compose logs -f --tail=200 pg-core     # 实时日志
```

健康 / 运维接口（宿主 8080）：

| 接口 | 作用 |
| --- | --- |
| GET /healthz | 进程 + 运行态快照（含 blocking_gates） |
| GET /readyz | 就绪（启动门禁是否全部通过） |
| GET /metrics | Prometheus 指标 |
| POST /admin/reload | 重新校验并热替换策略定义（运维面，不能下单/撤单/平仓） |

实测正常状态：

```json
{"process_healthy":true,"ready":true,"mode":"shadow","lease_healthy":true,
 "feeds_total":8,"feeds_connected":8,"events_total":698,"policy_decisions_total":0,
 "open_orders":0,"orders_journaled_total":0,"blocking_gates":[],"last_error":null}
```

## 2. 修了什么

### 2.1 阻塞运行的三个问题

| 问题 | 位置 | 说明 |
| --- | --- | --- |
| 仓库 HEAD 编译不过 | rust/crates/pg-core/src/health.rs | update(&mut self.inner.write().await) 类型不匹配（E0308）。上游 CI 对 HEAD 的结论就是 failure。 |
| cargo fmt --check 失败 | rust/crates/pg-control/* | CI 用浮动 stable，格式与仓库不一致。 |
| docker compose up 启动即崩溃 | docker-compose.yml | command 里重复写了二进制路径，实际命令行成了 /usr/local/bin/pg-core /usr/local/bin/pg-core --serve；第一个参数不是 --serve，于是走了「位置参数 = signal.json」分支，去把 ELF 当 JSON 读，报 stream did not contain valid UTF-8。 |

### 2.2 工程化加固

- 工具链固定到 1.98.1（Dockerfile + CI），不再随 stable 漂移。
- rust/Cargo.lock 现在提交入库，CI 用 cargo metadata --locked 校验它与 Cargo.toml 一致；镜像构建因此可复现。
- scripts/pull-base-images.ps1：通过镜像站拉基础镜像并 retag（见 §6）。
- scripts/rust-docker.ps1：本机没有 Rust 工具链时，在固定版本容器里跑任意 cargo 命令，target 缓存在命名卷里。make rust-docker 是它的封装。

## 3. 运行时：从「只打日志」到真正接通编排链

之前的 --serve 只把策略决策打一行日志就结束。现在每个决策都走完整链路：

```text
策略决策 -> pg-risk 风控门 -> OMS -> 先写日志(order.intent.persisted) -> 执行适配器
```

关键点：

- **新增影子执行适配器**（rust/crates/pg-execution/src/shadow.rs）：进程内模拟交易所，实现与 Hyperliquid/IBKR 相同的 ExecutionAdapter 契约。
  - 按 client order id 幂等：重放同一个 intent 会接管已存在的订单，不会重复下单。
  - reduce-only 软件护栏：平仓方向错误、或会「穿过零轴」的数量一律拒绝。
  - 已成交订单仍可按 client id 查到（对应 IBKR 的 open → completed → executions 三段式恢复语义）。
- **持仓反馈**：守护进程用每条行情给模拟交易所打标记价，并按策略真实持仓构造 PositionView（净头寸、均价、成交笔数、未实现收益、峰值收益）。**之前这个对象是一次构造、永远为 0 的常量**，所以退出规则永远不会触发。
- **每个定义只有一个决策引擎下单**：声明了 [[policy.*]] 规则图的定义由策略引擎负责下单，旧打分机的 Submit 会被抑制（debug 日志）。否则两个引擎会在同一标的上各下一单。
- **启动门禁**：11 个 StartupGate 现在都有真实的判定点，/healthz 会列出还卡在哪个 gate。
- **关停策略**：PG_SHUTDOWN_POLICY 真正生效（preserve / cancel_resting / flatten_owned），先撤挂单再平策略自有仓位。
- **热重载**：POST /admin/reload → 先全量校验再原子替换；任何一个文件不合法，正在运行的策略集保持不变。

### 仍然没做的（重要）

- **模拟成交不会回写 OMS**：影子交易所会成交，但持久化的 OrderRecord 仍停留在提交时的状态（Open / filled_quantity=0）。连续对账（reconcile_once）已实现且有单测，但守护进程还没有调用它。
- **没有连续对账循环**、没有 kill-9 / 网络分区 / 数据库故障注入证明。
- **Telegram 紧急平仓**仍未接线。
- **Binance Portfolio Margin** 适配器仍是占位实现。

## 4. 策略 / 自动化兼容性

### 4.1 长期存在、但在本机首次暴露的坑

1. **5 秒 K 线在 Hyperliquid 上不存在。** 在线因子默认 candle 是 5s，Hyperliquid 只支持 1m 及以上，于是该订阅被永久拒绝、ready 永远为 0。
   **现在**：pg-marketdata 提供各交易所的 K 线能力表；**显式**配置了交易所不支持的 [automation] candle_interval_ns 会在加载期直接失败并说明支持哪些周期；**不配置**时每个标的按自己交易所的最小周期取默认（Hyperliquid 1m、IBKR 5s）。这也让「一个模板跨多个交易所」重新成为可能。
2. **AutomatedStrategy 默认双向**：空仓时 score ≤ −entry_score 会直接开空。
   **现在**：新增 [strategy] allow_short，**默认 false（只做多）**，要开空必须显式声明。
3. **回放没有现成样本**：仓库里原本没有 data/ 目录。
   **现在**：data/replay/hype.jsonl（归一化 MarketEvent）与 data/replay/policy_features.jsonl（PolicyReplayFrame），配 data/replay/strategies/hype_policy.toml 与 data/replay/README.md。

```bash
make strategy-replay EVENTS=data/replay/hype.jsonl STRATEGY_DIR=../data/replay/strategies
make policy-replay FEATURES=data/replay/policy_features.jsonl STRATEGY_DIR=../data/replay/strategies
```

### 4.2 写策略时要知道的语义

- 规则图（filters/entries/exits）**只要引用的特征缺失就整体不成立**（fail-closed）。所以空仓时 position.unrealized_return 缺失会让整条退出规则失效——这正是「没有持仓就不该产生平仓单」。
- 退出规则独立于入场过滤器求值：入场被过滤掉不会影响已有持仓的管理。
- 引用未注册特征名会在**加载期**直接失败，不会静默变成缺失特征。
- [strategy] 的 id / order_quantity / entry_score / exit_score 都是必填。

## 5. IBKR：能看行情，还不能下单

- 新增 ibkr-marketdata cargo feature（镜像默认通过 PG_CORE_FEATURES 打开），pg-core --serve 现在可以把 IBKR 当作行情源：supported_shadow_venues() / feed_endpoint() / open_feed_stream()，validate_shadow_feeds 不再是 Hyperliquid 专用。
- 新增可选的 IB Gateway 容器（默认不启动）：

```bash
docker compose --profile ibkr up -d      # ghcr.io/gnzsnz/ib-gateway:stable
```

  实测该镜像 GATEWAY_OR_TWS=gateway：**paper 模式 API 端口 4002，live 模式 4001**——正好对应仓库默认的 IBKR_GATEWAY_ADDR=127.0.0.1:4002。默认 READ_ONLY_API=yes。

- 相关环境变量：IBKR_GATEWAY_ADDR、IBKR_CLIENT_ID、IBKR_ACCOUNT、IBKR_MARKET_DEPTH_ROWS、IBKR_DEFAULT_EXCHANGE、IBKR_DEFAULT_CURRENCY；网关侧 IBKR_TWS_USERID、IBKR_TWS_PASSWORD、IBKR_TRADING_MODE。
- **重要**：pg-ibkr 的执行适配器仍然没有被任何运行时构造。--serve 拒绝 paper/live，所有「下单」都发给进程内的模拟交易所。**IBKR 目前只能提供行情，不能交易。**
- 另外，pg-ibkr 包的是社区 ibapi crate，不是 IBKR 官方 Rust SDK。
- 合约映射是部署级的（默认 SMART/USD，可用 IBKR_DEFAULT_EXCHANGE/IBKR_DEFAULT_CURRENCY 覆盖）。FeedSpec 只带 venue + asset，逐标的的 con_id / 主交易所 / 非美股上市属于另一件事，目前不做推断。

## 6. 本机环境注意事项

- Docker 守护进程**连不上 Docker Hub**（auth.docker.io 超时），但宿主机和容器内的 crates.io / github.com 正常：

```powershell
./scripts/pull-base-images.ps1      # 走 docker.m.daocloud.io 拉取并 retag
```

- 本机没有 Rust 工具链，所以用 ./scripts/rust-docker.ps1 "<cargo 参数>"；脚本挂载整个仓库到 /src，所以 ../strategies、../data/replay 这些相对路径在容器里和在宿主上一致。
- 不要同时跑多个 pg-core 实例：每个 feed 各开一条 WebSocket，多实例叠加会触发 Hyperliquid 的连接限制，feeds_connected 会在 4~6 之间抖动、ready 永远为 0。单实例稳定 8/8。

## 7. 验证状态

本机在固定工具链容器里跑过：

| 检查 | 结果 |
| --- | --- |
| cargo fmt --check | 通过 |
| cargo clippy --workspace --all-targets -- -D warnings | 通过 |
| cargo test --workspace | 通过 |
| clippy -p pg-core --features ibkr-marketdata | 通过 |
| test -p pg-hyperliquid --features sdk | 通过 |
| test -p pg-ibkr --features sdk | 通过 |
| clippy -p pg-hyperliquid -p pg-ibkr --features sdk | 通过 |

运行时实测：docker compose up -d → 容器 (healthy)、8/8 行情源、/readyz 200、POST /admin/reload 202 并在日志中确认、make strategy-replay / make policy-replay 两个回放路径都按预期产出判定。

## 8. 建议的下一步

1. 先把你的策略写成 strategy.v1，用 make policy-replay 在离线样本上确认规则/阈值。
2. 影子模式观察：PG_SHADOW_FILL_MODE=immediate 能让模拟交易所成交，从而看到持仓/退出那一半逻辑；默认的 rest 只挂单不成交。
3. 接实盘前需要补完：连续对账把成交回写 OMS、崩溃窗口证明、紧急平仓、以及 IBKR/Hyperliquid 的真实执行适配器接线。
