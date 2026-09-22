# `pg-sim` JSONL 进程协议与跨语言验证

> 本协议只用于**无凭据离线模拟**。不是交易所适配器，不发送委托，不接入资金账户，不等同于真实 L2/L3 队列撮合。协议入口 `rust/crates/pg-sim/src/main.rs` 是 `v0.1.0-rc.1` 之后新增的 hardening 分支功能；验收以对应 exact-SHA CI 为准。

## 1. 真实源码边界

- `rust/crates/pg-sim/src/lib.rs`: `SimOrder`、`MatchingEngine::submit` / `match_once`、`MatchOutcome`、价格/订单类型的唯一 Rust 语义源。
- `rust/crates/pg-sim/src/main.rs`: stdin 一行 JSON 请求、stdout 一行 JSON 响应，逐行 flush；同一进程存续期间**每条请求新建一个** `MatchingEngine`，避免不同研究实验错误共享订单或仓位。
- `research/src/pg_tsy/sim/engine.py`: `RustSimClient` 管理持久子进程、单步/分批、单调递增 request ID，并严格核对回包 ID。
- `research/tests/test_sim_binary.py`: 真实 executable 经 `PG_SIM_BINARY` 注入后跑 `step`、70 请求 batch、无仓位 reduce-only、crossed book、无效订单测试。通用 Python-only CI 无 `PG_SIM_BINARY` 时可以跳过该**专用文件**，但跨语言 Gate 不允许跳过。

## 2. 启动和协议

```bash
cd rust
cargo metadata --locked --format-version 1 >/dev/null
cargo fmt --check
cargo test --locked -p pg-sim --all-targets
cargo clippy --locked -p pg-sim --all-targets -- -D warnings
cargo build --locked --release -p pg-sim --bin pg-sim
cd ../research
export PG_SIM_BINARY="$(pwd)/../rust/target/release/pg-sim"
test -x "$PG_SIM_BINARY"
python -m pytest -q tests/test_sim_binary.py -ra
```

一条合法 JSONL 请求，枚举字面量必须与 Rust serde schema 一致：

```json
{"request_id":1,"order":{"order_id":"f4c8c02e-5e3a-4076-8714-69137e7dcd2e","side":"Buy","kind":"Limit","quantity":"2","limit_price":"101","time_in_force":"Ioc","expire_at_ns":null,"post_only":false,"reduce_only":false,"display_quantity":null,"contingency":null},"top":{"bid_price":"99","bid_quantity":"4","ask_price":"100","ask_quantity":"3"},"position":"0","now_ns":42,"session":"CONTINUOUS"}
```

成功回包形状（`state/outcome` 以实际 `MatchingEngine` 为准）：

```json
{"request_id":1,"result":{"ok":true,"outcome":"Filled","state":"Filled","order_id":"f4c8c02e-5e3a-4076-8714-69137e7dcd2e","filled_quantity":"2","remaining_quantity":"0","fill":{"quantity":"2","price":"100"},"reason":null}}
```

失败回包保持 `request_id`（无效 JSON 没法恢复合法 ID 时为 `null`），`result.ok=false`，附 `error`；不会凭空输出 `fill`。例如 bid 大于 ask、价格不正、size 为负时拒绝；未知 session 拒绝。单行长度上限 1 MiB；当前实现先读取整行再检查长度，故**不能**将它描述为输入内存严格上界，若作为不可信公网入口必须改为 bounded reader，本工具不暴露网络监听器。

## 3. 数据类型与严格语义

| 字段 | 类型 | 不变量 |
| --- | --- | --- |
| `request_id` | 无符号整数 | 每个请求回原 ID，Python 客户端要比对；顺序消费 stdout |
| `order.order_id` | UUID | 每个 counterfactual 本次请求独立；不等于实盘 client ID |
| `side` | `Buy`/`Sell` | 区分仓位符号方向 |
| `kind` | `Market`/`Limit` | 限价单需有效 `limit_price` |
| `quantity`,`limit_price`,`top.*`,`position` | 十进制，JSON string | Rust 使用 Decimal；不可偷偷改成 float 作为财务真相 |
| `time_in_force` | `Ioc/Fok/Gtc/Gtd/Day/AtTheOpen/AtTheClose` 对应 Rust 变体 | venue 真实适配器必须另声明自己支持哪些订单类型 |
| `post_only`、`reduce_only` | bool | reduce-only 不允许由平仓跨越零仓位形成反向敞口 |
| `display_quantity`,`contingency` | 与 Rust `SimOrder` schema 对齐 | 跨请求不共享 engine；不能声称持久 OCO/OTO/OUO 管理 |
| `top` | BBO/可见深度的快照 | bid>ask、非正价格、负 size 直接拒绝 |
| `session` | `PRE_OPEN/OPENING/CONTINUOUS/CLOSING/CLOSED` | 不支持的值 fail closed |

程序是持续 stdin/stdout 服务：读到 EOF 自然退出；标准输出必须保持机器可解析 JSON，不输出临时日志/进度；日志应使用 stderr。Python 客户端一次发送、读取/校验一次，batch 可以一次进程进行多个请求；**进程持久并不代表订单簿持久**。

## 4. 回归与不可声称的能力

最低自动验收：Rust 9+4 现有+新增单测成功，`-D warnings` 通过、release binary 存在、Python 实际启动子进程并验证 70 条 batch、每条 identical 回包、`request_id` 严格递增匹配；无持仓 reduce-only 不产生 fill；交叉 BBO 和不完整订单返回 error。新增 bug 应先添加夹具再修改语义。

**明确不包含：** 真实交易所流动性和队列优先级、真实 L3 matching、撮合延迟、订单手续费/滑点校准、跨请求订单状态、真实 post-only/减仓 capability、真实盈利。任何 HFT performance 或对比 Jev 的 alpha 论断应采用独立数据集、时钟对齐和真实成交证据。新增 `main.rs` 修复的是‘Python 预期 binary 却不存在’的工程断链，**不是生产 OMS 闭环**。
