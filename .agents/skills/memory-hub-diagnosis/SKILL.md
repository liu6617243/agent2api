---
name: memory-hub-diagnosis
description: Agent2API 记忆中枢「看起来没生效」的排查规范 —— 已接入却没有调用记录、库里条数对不上、导入写了但界面读不出、检索搜不到历史、自动入库没跑、水位卡住、索引一致性告警。凡涉及「MCP 记忆 / 记忆中枢 / 知识库 / 记忆没进来 / 搜不到 / 条数不对」都按本 Skill 分层取证，不要凭一个绿灯推断另一个环节正常。
---

# 记忆中枢排查（分层取证）

> 核心口径：**连接、调用、入库、可读是四件独立的事。**
> 任何一处为空都要单独查，不能用另一处的绿推断这里也正常。

## 0. 一句话

先分层定位在哪一层断了，再动代码。层错了，改得越多越乱。

```
① 配置层  mcp.json 里有条目、服务在监听
② 协议层  initialize 成功、tools/list 有工具
③ 调用层  agent_beat 有行（只在真调用 memory_* 时写）
④ 写入层  l1 文件真的落了、条数对得上
⑤ 可读层  load_all() / 索引能读出来、界面数量一致
```

## 1. 分层取证命令

### ① 配置层

```bash
cat ~/.workbuddy/mcp.json | grep -A6 agent2api-memory
netstat -ano | findstr :3065          # Windows
lsof -iTCP:3065 -sTCP:LISTEN          # macOS
```

要看到 `"url": "http://127.0.0.1:3065/mcp/memory"` 与 `X-Agent` 头。
**缺 `X-Agent` 不算没接上，但记忆会记到共用的 `mcp` 名下**，接入页看不出来是谁写的。

### ② 协议层

```bash
curl -s -X POST http://127.0.0.1:3065/mcp/memory \
  -H "Authorization: Bearer <key>" -H "X-Agent: probe" \
  -H "Content-Type: application/json" -H "Accept: application/json, text/event-stream" \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}'
```

成功只证明「客户端能发现工具」。**这一步不写任何台账**，别拿它当「在用」的证据。

### ③ 调用层

```sql
-- 记忆中枢索引库里的 agent_beat
SELECT agent, calls, last_tool, writes, reads, errors FROM agent_beat ORDER BY last_seen DESC;
```

- 0 行 = 从来没有真正调用过（握手/列工具都不算）。
- 想验证链路是否活着：手动调一次只读工具（`memory_status` / `memory_core`），
  再查这张表 —— 立刻出现一行就说明心跳写入正常。
- **手动那条会污染统计**，回答用户时必须说明「这条是诊断触发的」。

WorkBuddy 里 memory 工具是 **deferred** 的：`request_raw.tools` 的 direct 数组里
没有 `memory_*`，但 `available_deferred_tools` 里有。看到 direct 数组没有就判
「工具没暴露」是误判。

### ④ 写入层

```bash
ls ~/.agent2api/memory/                 # 或配置的 memory.root
find <root>/l1 -name "*.md" | wc -l
find <root>/l1 -name "*.md" -size +1M -exec ls -lh {} \;
```

- 条数对不上先数**磁盘 marker**，不要先看界面。
- 单个 l1 容器异常大（几十 MB）→ 见 §3 容器归位。
- `import.sources` 里的路径是「可读来源」；`~/.workbuddy/projects` 本机常有几十个
  项目目录、上千个 `.jsonl`。

### ⑤ 可读层

```bash
# 界面「索引一致性需处理」→ 动作是「重建索引」，不是改配置
grep -i "memory" <logs> | grep -i "索引\|rebuild\|load_all"
```

判据必须是「点了修复按钮之后必然成立的等式」：`index.count == entries.len()`。
早先用「索引行数 vs live 条数」，有回收站条目的机器每次启动都全量重建，
重建写回同一批行，下次继续不一致 —— 自我维持的无效循环。

## 2. 「已接入但没有调用记录」的标准回答

三层结论，缺一层都不完整：

1. `agent_beat` 只在 `memory_mcp::call()` 真被调用时写；`initialize` / `tools/list` 不算。
2. 手动调一次立刻出现记录 → 心跳链路正常，之前为 0 就是**模型没有真正调用**。
   工具是 deferred 的，且 `~/.workbuddy/CODEBUDDY.md` 的受控块不一定进 system prompt
   —— 查 system prompt 里有没有 `memory_core` / `记忆中枢` 字样，别假设写了就会生效。
3. 即使模型一次都不调，日常会话也该进库 —— 靠 `schedule` 的 `ingest` 任务，
   不靠模型自觉。若 `ingest` 没跑，查 `import.autoIngest` 总闸与 `auto.tasks.ingest`。

## 3. 容器归位（导入 2341 条但普通记忆 0 条）

根因链：`entry_path()` 用 `created_at.max(updated_at)` → 所有历史挤进同一天容器 →
文件涨到几十 MB → 被 `storage.maxFileSizeKB` 整份跳过 → `load_all()` 看不到 L1。

口径：
- l1 容器是「一天所有记忆拼起来」的**容器**，不是用户扔进来的巨型单文件；
  体积上限只对非容器 `.md` 生效。
- 写入路径按**原始 `created_at`** 分日期，不按 `updated_at`。
- 存量坏数据必须迁移：`split_misplaced_containers()` 按每节 `created` 日期重新分组，
  先写成功再摘源，失败退回源容器；延迟落盘窗口打开时整体退让。

## 4. 增量入库的水位（最容易写错的地方）

```rust
fn next_watermark(processed_to: i64, since: i64, started: i64) -> Option<i64> {
    if processed_to <= 0 { return None; }
    Some(processed_to.saturating_sub(1).max(since).min(started))
}
```

| 坑 | 后果 |
| --- | --- |
| 用开跑时刻当水位 | 超上限那批被永久标成「已读」，静默漏记忆 |
| 不减 1ms | 同 mtime 兄弟文件被 `mtime > 水位` 挡掉 |
| 允许回退 | 同步工具改早 mtime 后每轮重扫 |
| 越过开跑时刻 | 跑的过程中新写的被划成已读 |
| 多来源取 max | 落后来源永远追不上（要用最落后来源的已处理位置） |
| 超限时优先取最新 | 积压永远清不掉（要按旧到新取） |

排查信号：日志里出现「有 N 个文件本轮没轮到」= 有积压，此时**必须**能在
`lastAuto.deferred` 与审计文案里看到它，否则「真有积压」与「真没新数据」长得一样。

## 5. 归因与过滤不能混用一个字段

`query.agent` = 过滤（只看某个客户端写的记忆）；`caller` = 归因（这次是谁在查）。
拿 `X-Agent` 给 `query.agent` 兜底，会把「搜全部」悄悄改成「只搜我自己写的」，
导入来的历史记忆一条都搜不到，而且不报错。

对外入口已拆：`list_for` / `get_for` / `recent_for` / `core_text_for` 单独接 caller。

## 6. 「打开记忆页转半天」怎么查

现象：点记忆中枢，卡在「正在读取记忆库…」好几秒。AgentHub 里很快就出数据。

先量化，别猜。本机实测基线（65 个 `.md` / 55 MB）：

| 动作 | 耗时 |
| --- | --- |
| 全量 `load_all()`（读 + 切节 + 每节 SHA-256） | 300~530 ms |
| 一屏打三遍全量（`/config` + `/projects` + `status`） | ~900 ms |
| 目录指纹 stat 遍历（快照命中） | ~8 ms |
| **13 个客户端各跑一次 `paths::detect`** | **2~3 秒** |

最后一行才是主犯：`detect()` 里的 `find_binary` 会把 PATH 上每个目录 ×
每个可执行名 × 每个 PATHEXT 后缀各 stat 一次（本机 58 × 11 ≈ 4.4 万次）。

### 四个必须一起看的点

| 点 | 症状 | 判据 |
| --- | --- | --- |
| `paths::detect` 缓存是 `thread_local` | axum 多线程，每条 worker 各自冷启动，TTL 形同虚设 | 缓存容器是 `thread_local!` + `RefCell` → 改成进程级 `Mutex<HashMap>` |
| `resources::for_client` 走完整 `detect` | 只要一个路径却扫全盘 | 有没有 `paths::config_path()` 这条浅通道 |
| `status()` 带 `table_stats()` | 10 张表各一次 `COUNT(*)`，`mem_link` 可达千万行 | 前端有没有真的读 `tables`（本仓：零消费） |
| `snapshot()` 冷启动没人预热 | 55 MB 解析由第一个开页面的用户付 | 启动后台有没有 `store::warm()` |

### 一条经验：前端首屏不能等「重接口」

`booted` 只该等**决定页面骨架**的那一个请求（`/config`：有哪些页签、默认落哪页）。
项目列表、体检、表统计都属于「页面画出来之后再补」——把它们放进 `Promise.all`
等于让最慢的那个决定首屏。

### 派生表也会撑爆索引库

`mem_link` 是派生的，`rebuild()` 却从来不清它 —— 每次重建都在旧图上叠加。
老 `relink()` 按「同项目 + 任意共同标签」连线，5818 条带 `导入` 标签的记忆
两两相连 → 810 万行 / 275 MB。

- 泛标签（出现在 >5% 条目上的标签）不能当主题；
- 每条记忆最多连 5 条边（`MEM_LINK_FANOUT`）；
- 重建前必须 `clear_links()`；
- 反向查邻居要有 `mem_link(to_id)` 索引，否则 `WHERE to_id = ?` 是全表扫。

## 7. 交付

改动完照 `delivery-pipeline` 走：

```bash
node scripts/precheck-rust.mjs && node scripts/precheck-knobs.mjs
node scripts/deliver.mjs
```

**本机常常没有 cargo**（`cargo: command not found`）：静态预检能过不等于能编译，
必须靠 CI 兜。别因为预检绿了就宣布改完了。
