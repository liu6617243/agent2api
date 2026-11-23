# AgentHub 记忆迁到 agent2api：可行性与产量评估

> 结论先说：**可行**，但「几万条」这个数字是误会 —— 26,188 条里 98.6% 是会话流水切片，
> 真正值得搬的蒸馏条目只有 **331 条**。全量搬可以做到，但等于给自己的检索池灌噪声。
>
> 工具已就绪：`scripts/migrate-agenthub.mjs`（干跑默认，`--apply` 才真写，带回滚台账）。

## 1. 源侧到底有多少东西

数据落在 `~/AgentHub/memory`：正文是 Markdown，`index/memory.sqlite` 是索引。
索引 `mem` 表 **26,188 行**，每行 = 一条记忆（一个 section），不是一篇文件。

| 维度 | 数量 |
|---|---|
| `mem` 表总行数 | 26,188 |
| 有效（未被替换 / 未过期） | 26,184 |
| `type = daily`（会话流水） | **25,821（98.6%）** |
| `layer = l2`（蒸馏品） | **331** |
| 带明确类型的 l1 笔记 | 36 |
| 磁盘上的 md 文件 | 904（l2 331 / l1 264 / profile 18 / reports 291） |
| 正文体量 | 约 13.5 MB |

`daily` 那 25,821 条的真实成分（按正文长度）：

| 长度 | 条数 | 说明 |
|---|---|---|
| < 200 B | 16,849 | 一句话、一条命令、一次工具调用 |
| 200 B – 1 KB | 7,154 | 单轮问答 |
| 1 – 4 KB | 1,390 | 有内容的一段排查 |
| > 4 KB | 428 | 真正成段的记录 |

蒸馏品（这 331 条才是「记忆」）：

| type | 条数 | 正文 |
|---|---|---|
| knowledge | 194 | 136 KB |
| decision | 130 | 70 KB |
| insight | 7 | 11 KB |
| 合计 | **331** | 217 KB |

> 顺带一个事实：`dedup_status` 里 25,973 条还是 `pending`，`dup_index` 全是 0 ——
> 也就是说 AgentHub 自己的去重**从没跑过这批数据**，那两万多条里还叠着大量近似重复。

## 2. 为什么不能「指个目录让导入页读」

agent2api 的 Markdown 导入是**一个文件一条**（`import.rs::from_markdown`），单文件正文上限
`MAX_FILE_CHARS = 24_000`。而 AgentHub 是「一天一个大文件、每个 section 一条」：

```
general/l1/workbuddy/2026-10-18.md   ← 1,361 条 section 挤在 1 个文件里
general/l1/workbuddy/2026-10-14.md   ← 1,157 条
```

直接指目录的后果：26,188 条 → 264 条，94 个文件被 24k 截断，同一天的几十条糊成一条。
所以必须**按 section 拆**（认 `<!-- mem:<id> -->` 锚点），逐条走 `POST /api/memory/write`。

## 3. 三种口径的实际产量

`node scripts/migrate-agenthub.mjs` 干跑的真实输出：

| 口径 | 条数 | 正文 | ≈tokens | 与现有撞车 |
|---|---|---|---|---|
| `--scope l2`（默认） | **331** | 143 KB | 66,560 | 0 |
| `--scope typed` | **363** | 174 KB | 81,070 | 0 |
| `--scope all` | **25,869**（315 条正文太短被丢） | 7.2 MB | 3,369,040 | 0 |

默认口径 `l2` 的落点分布：

| 项目 slug | 条数 |
|---|---|
| agenthub | 105 |
| 全局（`_global`） | 96 |
| agent2api-main | 63 |
| cbtc-ats | 48 |
| json-formatter | 18 |
| workbuddy-skill-absorption | 1 |

迁完之后的库面貌（agent2api 现状 l1 185 / l2 54 / profile 16 / 总 363）：

| | 现状 | 迁 331 条后 |
|---|---|---|
| l2 | 54 | **385** |
| 总数 | 363 | **694** |

**请求级成本不变。** 注入是固定预算（必须遵守 200 / 相关经验 260 / 长期画像 140，
合计 600 token），条目变多只是候选池变大，单次注入的 token 还是那 600。
真正的代价是**竞争**：385 条 l2 抢 260 token 的经验位，同主题的老条目更难浮上来。

## 4. 已验证：真写一条

```
node scripts/migrate-agenthub.mjs --scope typed --only l2:insight --limit 1 --apply
→ 成功 1 / 失败 0
→ 新 id 20261015-0406-f23316  l2 insight  importance 0.95  tags 9 个
→ createdAt 回填成 2026-10-15，project 归一化成 cbtc-ats
→ 验证后已 POST /api/memory/delete 删除（trashed: true）
```

链路、字段映射、时间回填、项目归一化、回滚台账都过了。

## 5. 字段映射（关键几处）

| AgentHub | agent2api | 说明 |
|---|---|---|
| `type=knowledge/decision/insight` | 同名 | → l2 |
| `lesson / bugfix / troubleshooting / debug` | `pitfall` | AgentHub 分四类，这边只一类 |
| `note / fact / conclusion / config / pattern / methodology / 技术结论 / technical` | `knowledge` | 认不出的归 knowledge，不归 other —— other 在注入里只值一档权重，等于白搬 |
| `importance 1..5` | 1→0.15 / 2→0.4 / 3→0.62 / 4→0.82 / 5→0.95 | 直接除 5 会让占 78% 的「3」压到 0.6，永远排不过自家蒸馏出来的 0.9 |
| `created`（ISO） | `createdAt`（ms） | 后端支持 `createdAt` 回填，历史时间不会丢 |
| `project: liu6617243--agent2api` | `agent2api-main` | 显式映射表 |
| `project: 南昌地铁…` | `cbtc-ats` | 后端 slug 只留 `[a-z0-9._-]`，中文会塌成空 → 必须显式给英文 |
| `project: 磁盘清理`（纯中文） | `disk-cleanup` | 同上，否则整个项目塌进 `_global` |
| `pinned / starred` | 同名 | AgentHub 里 0 条 pinned，实际不起作用 |

## 6. 建议

1. **先搬 `--scope l2`（331 条）**，这一步只增 143 KB、66 k token，收益最确定。
2. **不搬 daily**。agent2api 自己就在记 l1 流水；把 25,821 条压进来，等于把「有信息量的那一天」
   稀释成 1/100，而注入位只有 260 token。真要救某几天的记录，用
   `--only l1:daily --limit` 按日期挑，别整批。
3. **搬完看 Inject 预览**。`设置 · 注入预览` 会显示三段预算与每条的段标签；如果 agenthub 那批
   knowledge 把 preference 挤出去了，把导入条目的 importance 整体下调 0.1（改
   `IMPORTANCE` 表）再重跑，或把它们 `pinned=false` 保持现状（默认就不进「必须遵守」段）。
4. **想撤销**：台账在 `.workbuddy/tmp/agenthub-migrate-*.json`，每条都记了 `source → id`，
   `node scripts/migrate-agenthub.mjs --undo --apply` 一条命令全退回回收站。

## 7. 命令速查

```bash
# 干跑，看数（默认 l2）
node scripts/migrate-agenthub.mjs

# 干跑，363 条口径
node scripts/migrate-agenthub.mjs --scope typed

# 真写（推荐先按类试水）
node scripts/migrate-agenthub.mjs --only l2:decision --apply
node scripts/migrate-agenthub.mjs --apply

# 只搬前 N 条
node scripts/migrate-agenthub.mjs --apply --limit 20

# 只锁画像，不搬条目（--protect-only 必加：不加会顺带跑默认 l2 口径）
node scripts/migrate-agenthub.mjs --protect-profiles --protect-only --apply

# 锁画像 + 同一轮搬 l2（合起来用才省略 --protect-only）
node scripts/migrate-agenthub.mjs --protect-profiles --apply

# 撤掉一轮（默认取最近一份台账，进回收站，可 restore）
node scripts/migrate-agenthub.mjs --undo
node scripts/migrate-agenthub.mjs --undo --apply
node scripts/migrate-agenthub.mjs --undo --manifest .workbuddy/tmp/agenthub-migrate-2026....json --apply
```

`--undo` 走的是 `POST /api/memory/delete`，即**进回收站**而不是物理删除：撤错了还能对同一批 id 逐条 `POST /api/memory/restore` 捞回来。往返已实测过一轮（写 1 条 → 活跃 3 条；`--undo --apply` → 活跃 1 条，2 条在回收站），台账见 `.workbuddy/tmp/_undo-roundtrip.json`。

## 8. 迁完之后「深层记忆」和「画像」跟 AgentHub 一样吗

**深层记忆：正文逐字一样，外围元数据不一样。画像：完全不一样，而且会被改写。**

### 8.1 l2 深层记忆 —— 正文零损失

对 331 条逐条比对（原 body 为 `path` 指向文件的正文，迁移后为 `content` 字段）：

| 指标 | 结果 |
|---|---|
| 原正文合计 | 146,432 字符 |
| 迁移后正文合计 | 146,432 字符（**减少 0.0%**） |
| 发生裁剪的条目 | 0 |
| 被剥掉的 markdown 标题 | 0 个（l2 正文本来就没有 `#` 行） |

331 条 l2 的正文是**原样**搬过去的 —— 剥掉的只有 frontmatter（那是元数据，
不是正文）和 3 个以上连续空行的压缩。

元数据则按目标库的口径重写：

| 字段 | 变化 |
|---|---|
| `type` | knowledge / decision / insight 同名；`lesson`/`bugfix`/`troubleshooting`/`debug` 四类并成 `pitfall`（只发生在 typed 口径） |
| `layer` | l2 的这三类保持 l2；pitfall / preference 来自 l1 笔记时保持 l1 |
| `importance` | 1..5 重标定为 0.15 / 0.4 / 0.62 / 0.82 / 0.95（本批：1 条 0.15、126 条 0.62、167 条 0.82、37 条 0.95） |
| `project` | 归一到 agent2api 的 slug（`liu6617243--agent2api` → `agent2api-main`，中文项目 → 显式英文） |
| `agent` | `workbuddy` / `distill` / `trae` → `agenthub-migrate`（谁搬的，不是谁写的） |
| `created_at` | 用 `created` 回填（原时间保住） |
| `updated_at` | **= 迁移时刻**（后端行为：`createdAt` 只影响 `created_at` / `valid_from`） |
| `summary` | **丢**。agent2api 没有这个字段，列表与注入用 `title` + 正文开头代替 |
| `refs` / `cwd` / `git` / `session` / `device` / `projectName` | **丢**。agent2api 有 `files` / `evidence`，但 AgentHub 的 `refs` 是空数组，没有可搬的内容 |
| `validFrom` / `supersededBy` | 只有 4 条被替换过的，本来就被排除；其余为空 |

一句话：**打开某条记忆，正文一个字都不差；看它「属于谁、多重要、归哪个项目」，
是按新库的规矩重算的。**

### 8.2 画像 —— 不搬，而且会间接被改写

**不搬。** 脚本没有 profile 通道，`--scope` 的三个口径都不含 `type=profile`（源库里也没有
这个 type：AgentHub 的画像不是条目，是 `profile/` 下 4 个**渲染产物**
（`persona.md` / `preferences.md` / `habits.md` / `tech.md`），每个就是一串 bullet + 证据 id）。

**结构本来就不同：**

| | AgentHub | agent2api |
|---|---|---|
| 形态 | 4 个 md 文件（persona / preferences / habits / tech） | 16 条独立条目，按 4 个分组（`PROFILE_SUBS`） |
| 每条 | bullet + `证据 [n]：id, id` | 一句话 body + `evidence` 数组 |
| 生成 | 自己的 LLM 提示，直接覆盖写文件 | `enrich.rs::build_profile`，LLM 输出 JSON，旧结论进回收站 |
| 注入 | 整段 persona 塞进系统提示 | 只取 2 条（`PROFILE_MAX_ITEMS`），预算 140 token |

**会被间接改写，这是真风险。** 迁移把 331 条的 `updated_at` 写成「现在」，于是：

1. `freshSinceProfile` 从 60 涨到 391 → 越过 `deep.personaMinMemories`（默认 30）的门槛；
2. 下一次 `build_profile` 的素材池按 `importance*0.5 + recency*0.3 + pinned*0.2` 取前
   `deep.batchSize`（默认 50）条；
3. agenthub 那 37 条 importance 5 的分值是 `0.95*0.5 + 1.0*0.3 = 0.775`，**盖过现有
   0.9 分条目的 0.747** —— 50 条素材里预计 40 条左右会换成它的内容；
4. 没锁的旧画像进回收站，被新生成的替换。

**对策（已内置）：**

```bash
# 先把现有 16 条画像锁住（pinned + tags 加 pin，重建时跳过），不搬任何条目
node scripts/migrate-agenthub.mjs --protect-profiles --protect-only --apply
```

不锁也行，但要知道后果：迁完之后你的「画像」会从「agent2api 这几天自己的观察」
变成「agenthub 那批素材的再蒸馏」，两者措辞和侧重都不同，而且旧版只能在回收站里找。

### 8.3 想让画像跟 AgentHub 一样怎么办

搬不到一模一样 —— 那 4 个 md（persona / preferences / habits / tech）是 AgentHub 自己提示词的产物。想要「接近」，只有两条路：

1. **迁完手动跑一次画像生成**，让 agent2api 用这批素材重画，再看 `设置 · 注入预览` 的
   「长期画像」段是否满意；不满意就把 `deep.batchSize` 调小（比如 20），让素材池只吃
   最近的、最重的那些，再把新画像里对的条目 pin 住。
2. **把 AgentHub 那 4 个 md 当笔记搬**：它们在源库里就是 `type=note` 的 l1 行，
   已经包含在 `--scope typed` 的 32 条 l1 里（按 `note` → `knowledge` 写）。
   注意它们正文里带「证据 [n]：file_xxx, mem_xxx」这类内部 id，迁过去是纯文本噪声，
   不会自动变成 `evidence` 字段 —— 要干净就手抄一遍。
   这样它们进「相关经验」，**不进画像**，不会冒充「关于你的结论」，但内容仍能被检索到。

## 9. 那两万多条为什么不算进「331 条」

**它们是 l1，从来没被排除在记忆之外 —— 「331」是搬多少的口径，不是分类结论。**

源库分层实测（`select layer, type, count(*) from mem group by 1,2`）：

| 层 | 类型 | 条数 |
|---|---|---|
| l1 | daily | 25,821 |
| l1 | note / lesson / troubleshooting / methodology / fact / debug / bugfix / 技术结论 / technical / preference / pattern / finding / config / conclusion | 36 |
| l2 | knowledge | 194 |
| l2 | decision | 130 |
| l2 | insight | 7 |

l1 合计 25,857，l2 合计 331。`--scope all` 就是把这 25,869 条全搬，
`--scope l2` 只是搬蒸馏层。两边都把 l1 计入「普通记忆」总数，这一点没错。

**不默认搬的理由是 l1 的角色，不是它的身份：**

1. **形态**：daily 平均 537 字节，16,849 条不足 200 字节（一句话、一条命令、一次工具调用）。
   标题层面 258 条都叫 `<conversation_history_summary>`、19 条 `## 结论`、15 条 `{`；
   正文前 40 字有 215 条完全相同。agent2api 的列表和注入都拿 title 当检索键，这批条目
   的 title 不携带信息。
2. **体积**：正文 13.2MB，是 l2（143KB）的 96 倍。现在库里 l1 只有 188 条，搬完是 26,045 条。
3. **蒸馏回流**：`extract` 每 30 分钟吃「最新 20 条 l1」（`auto.tasks.extract` +
   `deep.extractThresholdCount`）。迁移把 `updated_at` 统一写成此刻，25,869 条全算新流水 →
   以 48 轮/天算，闸门会一直放行约 **27 天**，每轮都在把流水压成 l2 候选，
   把真正值钱的 331 条挤出「已有结论清单」（那份清单只保留 60 条）。
   粗算 1,294 轮 × 约 8k token ≈ **1000 万 token**。
4. **检索稀释**：`recallTopK 20 / finalTopK 8`。BM25 在 2.6 万条超短文本上 idf 塌陷，
   常见词一命中就把 20 个召回位占满。
5. **注入门槛**：普通 l1 要 3.3 分才进注入（路径命中 3.0 或两条独立线索 4.0 起算），
   daily 基本进不去 —— 搬它主要是让**检索**变吵，不是让**注入**变好。
6. **留存策略相反**：agent2api 自己的 `retain` 170 次尝试只落了 2 条，168 次因 `toolTurn`
   被跳过（工具轮流水主动不记）。整批搬 25,821 条 daily 等于把它刻意不记的东西塞回来。
7. **容器体积**：10-18 那天 1,467 条 / 711KB、10-14 那天 1,251 条 / 526KB，都压过
   `storage.maxFileSizeKB`（默认 512KB）。现在 l1 容器对这条上限是豁免的（store.rs 有回归
   测试钉着），但它本来就是「踩过坑才加豁免」的那个位置。

**真想搬，给两个削峰旋钮（本次新增）：**

```bash
# 全搬（会触发上面第 3 条，记得先关 extract）
node scripts/migrate-agenthub.mjs --scope all --apply

# 只搬正文 ≥512 字的（daily 里信息密度最高的那批）
node scripts/migrate-agenthub.mjs --scope all --min-size 512

# 单日最多 100 条，避免把 10-18 做成 700KB 的巨型容器
node scripts/migrate-agenthub.mjs --scope all --max-per-day 100
```

实测产量：`--min-size 500` → 1,261 条（4.9MB）；`--min-size 1024` → 611 条（4.5MB）；
`--min-size 2048` → 360 条（4.1MB）。条数掉得很快而体积掉得很慢，说明长尾是少数巨型
转写稿在占体积 —— 那批更适合当「笔记附件」，不适合当记忆条目。
