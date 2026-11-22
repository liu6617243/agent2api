//! 记忆中枢 · 配置 schema 与分层读写。
//!
//! ── 为什么要有「schema 表」而不是一堆 struct 字段 ─────────────
//! 记忆中枢的可调项有一百多个（存储 / 索引 / 检索权重 / 归类阈值 / 模型路由 /
//! 十八个自动化任务 …）。如果写成 struct，那么：加一个旋钮要改 struct、改
//! 前端类型、改三处 JSON 手编示例；删一个旋钮会让老配置文件整份解析失败。
//!
//! 所以这里只有一张**元数据表**（[`schema`]）：每个旋钮登记类型、默认值、范围、
//! 分组、层级（基础 / 高级 / 内部）、一句话说明。配置本身是**扁平的点号键值对**
//! （`{"agents.coreMaxTokens": 800}`），读取时按表补默认值，写入时只校验表里
//! 有的键 —— 未知键被静默丢弃，于是「老配置 + 新代码」「新配置 + 老代码」两个
//! 方向都不会崩。
//!
//! 前端那一侧不用抄第二份枚举：`GET /api/memory/config` 把整张表下发给页面，
//! 表单按 `kind` 自动渲染、按 `tier` 折叠。前端永远不需要知道「有哪些旋钮」。
//!
//! ── 为什么要 `default_json: &'static str` 而不是 `fn() -> Value` ──
//! 静态表要能写成 `static`。闭包不能进 const 上下文，指向具名函数的 fn 指针可以，
//! 但那样一百多个默认值得配一百多个具名函数。存 JSON 文本、用时 `from_str` 一次
//! （结果缓存在 OnceLock 里）是唯一既 const 又不用写样板的路子。
//!
//! ── 与 AgentHub 的关系 ──────────────────────────────────────
//! 键名、分组、默认值按 AgentHub 的 `config-schema.cjs` 对齐（磁盘上同一份
//! `config.json` 两边都能读），实现是本仓自己写的。

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use serde_json::{json, Map, Value};

use super::store;

/// 旋钮的取值类型。决定前端用什么控件、后端怎么校验。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 数字（可带 min / max / step）
    Number,
    /// 布尔（开关）
    Bool,
    /// 单行文本
    Text,
    /// 单行文本，但**落盘加密、回显只给「已设置」**（WebDAV 密码 / 口令）。
    ///
    /// 与 `Text` 分开的理由：加密是**存储层**的属性，不是界面的属性。若靠前端
    /// 记住「这个框别显示」，接口一旦被直接 curl 就泄露明文；做成一种 kind
    /// 之后，「`GET /api/memory/config` 不回显秘密」由后端一处保证。
    Secret,
    /// 路径（字符串，但前端给目录选择器）
    Path,
    /// 枚举（`options` 里选一个）
    Enum,
    /// 多选（`options` 里选若干）
    Multi,
    /// 字符串列表（自由输入）
    List,
    /// 任意对象（如 `auto.tasks`）
    Map,
}

/// 配置层级。前端按这个折叠：基础全展开、高级默认收起、内部只在排障时显示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Basic,
    Advanced,
    Internal,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Basic => "basic",
            Tier::Advanced => "advanced",
            Tier::Internal => "internal",
        }
    }
}

/// 一个旋钮的元数据。
pub struct Knob {
    /// 点号键名，如 `search.weightBm25`
    pub key: &'static str,
    pub kind: Kind,
    pub tier: Tier,
    /// 分组名（前端按它分卡片）
    pub group: &'static str,
    pub label: &'static str,
    /// 一句话说明。空 = 不需要解释
    pub desc: &'static str,
    /// 默认值的 JSON 文本
    pub default_json: &'static str,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// `Enum` / `Multi` 的可选值
    pub options: &'static [&'static str],
    /// 改了立刻生效（`false` = 需要重启或重建索引）
    pub hot: bool,
    /// 运行时代码**真的读它**（`false` = 只落盘、只显示，没有任何行为影响）。
    ///
    /// ── 为什么要这个字段，而不是把未接入的键直接删掉 ────────────
    /// 这张表是**跨实现的配置契约**：磁盘上同一份 `config.json` 两边都能读
    /// （见文件头）。删键会让「老配置 → 新代码」静默丢值，「新配置 → 老代码」
    /// 被当未知键丢弃；而保留一个能改、能存、看不出没接线的旋钮，是更糟的一种：
    /// 用户照着自己的理解调完、界面显示「已保存」、行为一点没变，然后开始怀疑
    /// 整个页面是不是假的。
    ///
    /// 所以未接入的键**留着、标出来**：`wired=false` 下发到前端 → 控件置灰 +
    /// 「本仓未接入」徽标；后端单测 `unwired_knobs_explain_themselves` 要求
    /// 每个未接入的键都在 `desc` 里写清「为什么没接线」，不允许出现「静默的
    /// 死旋钮」。同时 `wired_knobs_have_a_reader` 用源码扫描确保每个
    /// `wired=true` 的键都真的有读取点 —— 这条是防**新加水**，比防旧水更重要。
    pub wired: bool,
}

const fn knob(key: &'static str, kind: Kind, tier: Tier, group: &'static str, label: &'static str, default_json: &'static str) -> Knob {
    Knob { key, kind, tier, group, label, desc: "", default_json, min: None, max: None, options: &[], hot: true, wired: true }
}

const fn knob_d(key: &'static str, kind: Kind, tier: Tier, group: &'static str, label: &'static str, desc: &'static str, default_json: &'static str) -> Knob {
    Knob { key, kind, tier, group, label, desc, default_json, min: None, max: None, options: &[], hot: true, wired: true }
}

/// 标成「本仓未接入」：值照存、界面照显示，但不影响任何行为。
///
/// `desc` 必须由调用方写好「为什么」—— 用户看到灰掉的开关时需要的是原因，
/// 不是一句「暂不支持」。这条由单测 `unwired_knobs_explain_themselves` 钉住。
const fn unwired(mut item: Knob) -> Knob {
    item.wired = false;
    item
}

const fn range(mut item: Knob, min: f64, max: f64) -> Knob {
    item.min = Some(min);
    item.max = Some(max);
    item
}

const fn opts(mut item: Knob, options: &'static [&'static str]) -> Knob {
    item.options = options;
    item
}

const fn cold(mut item: Knob) -> Knob {
    item.hot = false;
    item
}

/// 全部旋钮。顺序即界面顺序。
static SCHEMA: &[Knob] = &[
    // ===== 存储 =====
    knob_d("storage.root", Kind::Path, Tier::Basic, "存储", "记忆根目录",
        "空 = 默认 {配置目录}/memory。改完下一次读取即生效；旧目录里的文件不会自动搬",
        "\"\""),
    knob_d("storage.backupBeforeWrite", Kind::Bool, Tier::Basic, "存储", "改写前备份 .bak",
        "", "true"),
    range(knob("storage.backupKeep", Kind::Number, Tier::Advanced, "存储", "备份保留份数", "5"), 1.0, 50.0),
    range(knob_d("storage.trashKeepDays", Kind::Number, Tier::Basic, "存储", "回收站保留天数",
        "到期后每天自动清理任务把文件从磁盘彻底删除", "30"), 7.0, 365.0),
    range(cold(knob_d("storage.maxFileSizeKB", Kind::Number, Tier::Internal, "存储", "单文件上限 (KB)",
        "读取时跳过超过这个体积的记忆文件（手写的巨型日志不该整份进索引）；0 附近的值会连正常长文一起跳掉", "512")), 16.0, 8192.0),

    // ===== 索引与检索 =====
    cold(range(knob_d("index.titleBoost", Kind::Number, Tier::Advanced, "索引", "标题权重倍数",
        "写入索引时标题重复的次数；改动需重建索引", "3"), 1.0, 10.0)),
    range(knob_d("search.recallTopK", Kind::Number, Tier::Advanced, "检索", "检索召回条数",
        "关键词检索最多取多少条候选再打分排序；调大更全但一次检索更慢", "20"), 5.0, 200.0),
    range(knob_d("search.finalTopK", Kind::Number, Tier::Basic, "检索", "最终返回条数",
        "MCP memory_search 不传 limit 时用它；界面列表自己分页，不受它影响", "8"), 1.0, 50.0),
    range(knob_d("search.timeDecayHalfLife", Kind::Number, Tier::Advanced, "检索", "时间衰减半衰期（天）",
        "0 = 不衰减", "30"), 0.0, 365.0),
    range(knob("search.weightBm25", Kind::Number, Tier::Internal, "检索", "权重·相关度", "0.5"), 0.0, 1.0),
    range(knob("search.weightRecency", Kind::Number, Tier::Internal, "检索", "权重·时间", "0.15"), 0.0, 1.0),
    range(knob("search.weightImportance", Kind::Number, Tier::Internal, "检索", "权重·重要度", "0.1"), 0.0, 1.0),
    range(knob("search.weightAffinity", Kind::Number, Tier::Internal, "检索", "权重·项目/Agent 亲和", "0.15"), 0.0, 1.0),
    range(knob("search.weightGraph", Kind::Number, Tier::Internal, "检索", "权重·图扩散", "0.05"), 0.0, 1.0),
    range(knob_d("search.weightHeat", Kind::Number, Tier::Internal, "检索", "权重·使用热度",
        "被检索命中的次数，0 = 关闭", "0.1"), 0.0, 1.0),
    range(knob("search.weightLayer", Kind::Number, Tier::Internal, "检索", "权重·层级", "0.05"), 0.0, 1.0),
    knob("search.synonymsEnabled", Kind::Bool, Tier::Basic, "检索", "同义词表扩展", "true"),
    range(knob("search.graphExpansionDepth", Kind::Number, Tier::Internal, "检索", "图扩散深度", "1"), 0.0, 3.0),
    range(knob("search.graphExpansionMax", Kind::Number, Tier::Internal, "检索", "图扩散最多补充条数", "5"), 0.0, 30.0),

    // ===== 归类 =====
    range(knob("classify.fuzzyThreshold", Kind::Number, Tier::Advanced, "归类", "模糊匹配阈值", "0.62"), 0.3, 1.0),
    knob_d("classify.gitPreferred", Kind::Bool, Tier::Basic, "归类", "Git 地址优先",
        "关掉后只按目录名与已登记项目匹配，不读工作目录的 git remote", "true"),
    knob_d("classify.autoCreateProject", Kind::Bool, Tier::Advanced, "归类", "模糊失败自动建新项目",
        "默认关，避免产生混乱的碎项目", "false"),

    // ===== Agent 接入 =====
    opts(knob("agents.enabled", Kind::Multi, Tier::Basic, "Agent 接入", "启用的 Agent",
        "[\"zcode\",\"codex\",\"workbuddy\",\"claude\",\"dsh\",\"trae-solo\",\"antigravity\"]"),
        &["zcode", "codex", "workbuddy", "claude", "dsh", "trae-solo", "cursor", "qoder", "qoder-cn", "antigravity"]),
    knob("agents.custom", Kind::List, Tier::Advanced, "Agent 接入", "自定义 Agent（本机）", "[]"),
    // 这两条是 AgentHub 的轮询式接入模型（定时探活 + 自动校验）留下的。本仓的
    // 接入是「用户点一下才握手」：没有后台巡检线程，也没有「刚接完就自动测一遍」
    // 的动作。保留键是为了与 AgentHub 共用同一份 config.json，标 unwired 让界面
    // 说清楚，而不是让人以为开了它就会有后台任务跑起来。
    unwired(knob_d("agents.autoVerify", Kind::Bool, Tier::Basic, "Agent 接入", "接入后自动校验",
        "本仓未接入：接入是手动的，没有「写入后自动握手」这一步；点接入页的「测试连接」等价", "true")),
    unwired(range(knob_d("agents.verifyInterval", Kind::Number, Tier::Advanced, "Agent 接入", "连接巡检间隔（秒）",
        "本仓未接入：没有后台巡检线程；探活只在点「测试连接 / 全部校验」时发生", "300"), 30.0, 3600.0)),
    knob_d("agents.injectAgentsMd", Kind::Bool, Tier::Advanced, "Agent 接入", "注入 AGENTS.md 受控块",
        "关掉则只在 MCP initialize.instructions 里给行为约定", "true"),
    range(knob("agents.coreMaxTokens", Kind::Number, Tier::Internal, "Agent 接入", "memory_core token 上限", "800"), 200.0, 4000.0),
    range(knob_d("agents.digestMaxLines", Kind::Number, Tier::Internal, "Agent 接入", "memory_digest 行数上限",
        "MCP memory_digest 不传 maxLines 时用它", "200"), 50.0, 1000.0),
    range(knob_d("agents.searchMaxTokens", Kind::Number, Tier::Internal, "Agent 接入", "memory_search token 上限",
        "检索结果超预算时从尾部丢行，直到估算 token 落回这个数", "1200"), 200.0, 4000.0),

    // ===== 请求级注入 =====
    //
    // ── 这一组解决的是什么 ──────────────────────────────────────
    // 记忆存了几千条、MCP 工具也能用，但模型**不会主动去查** —— 实测
    // `memory_core` 只被调过 1 次、真检索 0 次。靠提示词块写一句「开局先读记忆」
    // 是建议，客户端可以不读、模型可以忽略。
    //
    // 所以这里改成网关在转发前主动把「必须遵守 + 相关经验」塞进请求。模型不需要
    // 自觉，因为它一睁眼就看到了。整组默认开、预算小（600 token），关掉即恢复
    // 改造前的行为。
    knob_d("inject.enabled", Kind::Bool, Tier::Basic, "请求级注入", "转发前注入记忆",
        "把「必须遵守」与「相关经验」拼成一段 system 消息塞进每次对话；关掉则只保留 MCP 工具（模型需自己查）",
        "true"),
    range(knob_d("inject.maxTokens", Kind::Number, Tier::Basic, "请求级注入", "注入总预算（token）",
        "整块硬上限，两段加起来不许超过它", "600"), 100.0, 4000.0),
    range(knob_d("inject.rulesTokens", Kind::Number, Tier::Advanced, "请求级注入", "「必须遵守」预算",
        "置顶条目 + 偏好 + 画像这几类，用户明确表达过的长期要求", "260"), 0.0, 2000.0),
    range(knob_d("inject.experienceTokens", Kind::Number, Tier::Advanced, "请求级注入", "「相关经验」预算",
        "命中本次请求里的路径 / 标识符 / 命令名的避坑与决策", "340"), 0.0, 3000.0),
    range(knob_d("inject.maxItems", Kind::Number, Tier::Advanced, "请求级注入", "相关经验最多几条",
        "再多也不会全塞 —— 一次给 6 条以内的精度最高", "6"), 1.0, 20.0),
    knob_d("inject.pathMatch", Kind::Bool, Tier::Advanced, "请求级注入", "按文件路径精确命中",
        "记忆的 files 字段与请求里的路径做字符串匹配；权重最高、几乎不误报", "true"),

    // ── 请求后留存 ─────────────────────────────────────────────
    // 注入解决「记忆能被看见」，留存解决「新记忆能进来」。只做前者时，用户的感觉
    // 是「它记得老东西，但新东西永远不会自己记下来」—— 而靠客户端提示词让模型
    // 自觉调 `memory_core` 写，实测调用 1 次、真写入 0 次，跟「模型不会主动读」
    // 是同一个病根：该做的事被放在了有选择权的位置。
    //
    // 所以网关在响应结束后自己判断「这一轮值不值得记」。默认开，但有一道内容闸
    //（`retain.minChars` + 结论词判据）：逐轮写会把库灌成流水账，几个月后九成
    // 是「好的 / 谢谢」。
    knob_d("retain.enabled", Kind::Bool, Tier::Basic, "请求后留存", "回答结束后自动写回记忆",
        "把这一轮的用户输入与助手结论写成一条 l1；关掉则只能靠模型或手工调 memory_core 写",
        "true"),
    range(knob_d("retain.minChars", Kind::Number, Tier::Advanced, "请求后留存", "最短回答长度",
        "少于这么多字符的回答不记（挡掉「好的」「收到」这类寒暄）", "20"), 0.0, 2000.0),
    range(knob_d("retain.maxChars", Kind::Number, Tier::Advanced, "请求后留存", "单条正文上限",
        "一轮对话写进一条记忆时的字符上限，超出截断", "12000"), 500.0, 40000.0),

    // ===== 深层记忆 =====
    knob("deep.enabled", Kind::Bool, Tier::Basic, "深层记忆", "深层记忆总开关", "true"),
    knob("deep.personaEnabled", Kind::Bool, Tier::Basic, "深层记忆", "生成人格画像", "true"),
    range(knob("deep.personaMinMemories", Kind::Number, Tier::Advanced, "深层记忆", "画像最少记忆数", "30"), 5.0, 500.0),
    knob_d("deep.evidenceRequired", Kind::Bool, Tier::Advanced, "深层记忆", "画像结论必须附证据链",
        "关掉后画像可无 #hash 锚点", "true"),
    range(knob("deep.batchSize", Kind::Number, Tier::Advanced, "深层记忆", "单次处理条数", "50"), 5.0, 500.0),
    range(knob_d("deep.distillMaxPerProject", Kind::Number, Tier::Internal, "深层记忆", "单项目蒸馏素材上限",
        "一次蒸馏喂给模型多少条素材（按重要度取前 N 条）", "60"), 20.0, 1000.0),
    // ── 抽取的「攒够再跑」闸门 ────────────────────────────────
    // 只靠时间间隔不够：2 小时一轮 × 每轮 3 条 = 每天仍然能产 36 条结论，
    // 而真机一天的流水只有几十条 —— 等于每轮都在同一批素材上重复抽取。
    // AgentHub 的 `extract.thresholdCount` 是同一个问题同一份解：新增流水攒够
    // N 条才值得跑一次，否则这一轮必然只是把上次说过的话再说一遍。
    range(knob_d("deep.extractThresholdCount", Kind::Number, Tier::Advanced, "深层记忆",
        "抽取闸门：新增流水条数", "距上次抽取之后新增的流水不足这个数就跳过本轮（手动执行不受限）", "20"),
        3.0, 500.0),

    // ===== 双时间轴 =====
    knob("timeline.enabled", Kind::Bool, Tier::Advanced, "双时间轴", "启用双时间轴", "true"),
    knob("timeline.autoDetect", Kind::Bool, Tier::Advanced, "双时间轴", "蒸馏时自动识别取代", "true"),
    knob_d("timeline.requireConfirm", Kind::Bool, Tier::Advanced, "双时间轴", "失效判定需人工确认",
        "关掉后仅高置信建议自动应用", "true"),

    // ===== 自动化 =====
    knob("auto.enabled", Kind::Bool, Tier::Basic, "自动化", "自动化总开关", "true"),
    // ── 默认「不限」是刻意的 ────────────────────────────────
    // 原来默认 200000，结果每一个跑过自动化的人都收到一枚「今日预算已用完」，
    // 而他从来没要求过这个限制、界面上也没有一处说「这能关掉」。预算是**可选**
    // 项：想要上限的人自己填数字（填了才有 `auto.overBudgetAction` 这回事）。
    range(knob_d("auto.dailyTokenLimit", Kind::Number, Tier::Basic, "自动化", "日 token 上限",
        "0 = 不限（默认）；填了才在超限时按 overBudgetAction 处理", "0"), 0.0, 100_000_000.0),
    opts(knob_d("auto.overBudgetAction", Kind::Enum, Tier::Advanced, "自动化", "超预算行为",
        "仅当「日 token 上限」填了数字才生效；0 = 不限时永不触发", "\"pause\""),
        &["pause", "ignore"]),
    range(knob_d("auto.logKeepDays", Kind::Number, Tier::Internal, "自动化", "任务日志保留天数",
        "每天清理任务按它删掉过期的 llm_call / task_run 行；条数上限另见 logKeepCount", "30"), 1.0, 365.0),
    range(knob("auto.logKeepCount", Kind::Number, Tier::Internal, "自动化", "任务日志保留条数", "200"), 20.0, 2000.0),
    knob_d("auto.tasks", Kind::Map, Tier::Advanced, "自动化", "任务开关与节奏",
        "默认开：零/低消耗任务；耗 token 的任务默认关",
        "{\"extract\":{\"enabled\":true,\"intervalMin\":30,\"batchSize\":20},\
          \"summarize\":{\"enabled\":false,\"intervalMin\":30,\"batchSize\":20},\
          \"tag\":{\"enabled\":false,\"intervalMin\":30,\"batchSize\":20},\
          \"classify\":{\"enabled\":true,\"intervalMin\":60},\
          \"supersede\":{\"enabled\":false,\"daily\":\"11:00\",\"autoApplyConfidence\":0.9},\
          \"distill\":{\"enabled\":false,\"daily\":\"11:10\"},\
          \"consolidate\":{\"enabled\":false,\"weekly\":1,\"weeklyTime\":\"11:00\"},\
          \"profile\":{\"enabled\":false,\"weekly\":1,\"weeklyTime\":\"11:10\"},\
          \"index-scan\":{\"enabled\":true,\"intervalMin\":360},\
          \"cleanup\":{\"enabled\":true,\"daily\":\"11:20\"},\
          \"health\":{\"enabled\":true,\"daily\":\"11:30\"},\
          \"ingest\":{\"enabled\":true,\"intervalMin\":30}}"),

    // ===== 去重 =====
    //
    // ── L3 / L4 为什么整组都标 unwired ─────────────────────────
    // 本仓的去重只实现了两级：L1 精确哈希（`find_hash_duplicate`，写入前挡同一
    // 内容）与 L2 词面近似（`near_duplicate`，只产建议进待确认队列）。
    // L3（BM25 粗排候选）与 L4（让模型做 ADD/UPDATE/DELETE/NONE 四操作判定）
    // 需要一套带 `dedup_status` 的状态机 + 逐条模型的裁决流水，本仓没有。
    // 留键是为了 config.json 与 AgentHub 互通；标 unwired 是为了不让用户以为
    // 打开「L4 语义判定」就会开始耗 token 判重 —— 那件事没接。
    knob("dedup.enabled", Kind::Bool, Tier::Basic, "去重", "去重总开关", "true"),
    knob("dedup.l1.enabled", Kind::Bool, Tier::Basic, "去重", "L1 精确哈希去重", "true"),
    opts(knob_d("dedup.l1.normalizeLevel", Kind::Enum, Tier::Advanced, "去重", "归一化强度",
        "算内容哈希前怎么归一：full = 统一换行 + 去行尾空白 + 整体 trim；trim = 只 trim；none = 原样。改它会换掉整库哈希，需重建索引",
        "\"full\""),
        &["none", "trim", "full"]),
    knob("dedup.l2.enabled", Kind::Bool, Tier::Basic, "去重", "L2 文本近似去重", "true"),
    // 两条近似重复里留谁：默认由信息量决定，而不是「谁更新」。
    //
    // 关掉就退回旧规则（新条取代旧条）。留着这个开关是因为它改的是**已有数据**
    // 的去向：判错的话一条完整结论会被标成已取代（仍可在回收站/演化链里找回），
    // 用户需要一个不必改配置文件的退路。
    knob_d("dedup.l2.preferRicher", Kind::Bool, Tier::Advanced, "去重", "留信息量更大的那条",
        "开：两条近似重复时，正文更长 / 标签更全 / 带代码与证据的那条留下，另一条标为被取代。\
         关：退回「新条取代旧条」—— 隔天补一句「同上」就可能把完整结论埋掉", "true"),
    range(knob("dedup.l2.autoMergeThreshold", Kind::Number, Tier::Advanced, "去重", "L2 自动合并阈值", "0.9"), 0.5, 1.0),
    range(knob("dedup.l2.candidateThreshold", Kind::Number, Tier::Advanced, "去重", "L2 候选阈值", "0.72"), 0.3, 1.0),
    range(knob("dedup.l2.wDice", Kind::Number, Tier::Internal, "去重", "L2 集合相似度权重", "0.5"), 0.0, 1.0),
    range(knob("dedup.l2.wEdit", Kind::Number, Tier::Internal, "去重", "L2 编辑距离权重", "0.3"), 0.0, 1.0),
    range(knob("dedup.l2.wTitle", Kind::Number, Tier::Internal, "去重", "L2 标题权重", "0.2"), 0.0, 1.0),
    unwired(range(knob_d("dedup.l3.topK", Kind::Number, Tier::Advanced, "去重", "L3 BM25 候选条数",
        "本仓未接入 L3：没有 BM25 粗排这一级，L2 直接对同类型条目做词面相似度", "8"), 1.0, 30.0)),
    unwired(knob_d("dedup.l4.enabled", Kind::Bool, Tier::Basic, "去重", "L4 语义判定（耗 token）",
        "本仓未接入 L4：没有让模型做 ADD/UPDATE/DELETE/NONE 判定的流水；近似重复只产待确认建议", "false")),
    unwired(range(knob_d("dedup.l4.minCandidateScore", Kind::Number, Tier::Advanced, "去重", "L4 触发最低候选分",
        "本仓未接入 L4：没有候选分门槛这一环（L2 的门槛是 dedup.l2.candidateThreshold）", "0.62"), 0.0, 1.0)),
    range(knob_d("dedup.l4.autoUpdateThreshold", Kind::Number, Tier::Advanced, "去重", "L4 UPDATE 自动执行置信",
        "「事实失效」自动应用的最低置信度（失效判定任务用这个门槛，不是 L4 判重）", "0.8"), 0.0, 1.0),
    unwired(range(knob_d("dedup.l4.batchSize", Kind::Number, Tier::Internal, "去重", "L4 单批条数",
        "本仓未接入 L4：没有分批判重；单批条数由 auto.tasks 里各任务的 batchSize 决定", "20"), 1.0, 50.0)),
    range(knob("dedup.pendingWarnThreshold", Kind::Number, Tier::Advanced, "去重", "队列积压告警阈值", "50"), 1.0, 999.0),

    // ===== 待确认队列的自动确认（默认全关）=====
    knob_d("review.autoConfirmSupersede", Kind::Bool, Tier::Advanced, "待确认", "自动确认事实失效",
        "建议产生后立即按推荐标记旧事实失效，不再等人工点头", "false"),
    knob_d("review.autoConfirmClassify", Kind::Bool, Tier::Advanced, "待确认", "自动确认项目归类",
        "归类建议产生后立即按推荐归入对应项目", "false"),
    knob_d("review.autoConfirmDedup", Kind::Bool, Tier::Advanced, "待确认", "自动确认去重采纳",
        "去重建议产生后立即按推荐采纳新记忆；永不物理删除", "false"),
    // ── 蒸馏结论默认直接落地（与上面三条相反，故意的）──────────
    //
    // 上面三条默认关，因为它们改的是**已有记忆**（搬项目、标失效、合并重复），
    // 判错的代价是破坏原有数据。蒸馏不一样：它只**新增**一条 l2，来源写在
    // `supersedes` / `evidence` 里，删掉就回到原样 —— 而且 `deep.evidenceRequired`
    // 已经挡掉了「说不出出处」的结论。
    //
    // 默认开是为了补一个实测出来的死结：导入 2341 条之后，「深度记忆」还是 9 条。
    // 原因不是蒸馏没跑，而是蒸馏的产物全部停在待确认队列里等人点头，而人不会
    // 每天去点几百下。于是 l2 永远长不出来，而请求级注入最值钱的那一段
    // （沉淀结论 + 踩坑/决策）—— 注入看着「开了」，实际没东西可注。
    // 队列留着当审计，但不再当闸门。
    knob_d("review.autoConfirmDistill", Kind::Bool, Tier::Basic, "待确认", "蒸馏结论直接落地",
        "开：蒸馏产出的结论直接写成 l2（仍进队列留档，可随时删）；关：只入队等人工点头。\
         关掉后 l2 几乎不会自己长出来，请求级注入也就没东西可注", "true"),

    // ===== 清理（危险动作，默认全关）=====
    knob_d("cleanup.autoDeleteSuperseded", Kind::Bool, Tier::Advanced, "清理", "自动清理失效记忆",
        "被取代或已过有效期的记忆移入回收站（保留期内可恢复）", "false"),
    knob_d("cleanup.autoDeleteMissing", Kind::Bool, Tier::Advanced, "清理", "自动清理不存在记忆",
        "索引里还有、磁盘文件已丢失的记录，清除索引行", "false"),
    knob_d("cleanup.autoDeleteError", Kind::Bool, Tier::Advanced, "清理", "自动清理报错记忆",
        "文件存在但读取失败（权限/损坏）的，整份移入回收站", "false"),
    knob_d("cleanup.autoDeleteConflict", Kind::Bool, Tier::Advanced, "清理", "自动清理冲突记忆",
        "同步冲突未裁决的，整份移入回收站并清掉冲突记录", "false"),
    knob_d("cleanup.autoDeleteInvalid", Kind::Bool, Tier::Advanced, "清理", "自动清理无效记忆",
        "内容为空壳、进不了索引的残骸，整份移入回收站", "false"),

    // ===== 模型与路由 =====
    //
    // ── 本仓的模型选择只有一条链路（`llm::model_for`）───────────
    // 显式路由（models.routing）→ 思考强度对应的标签（models.models 里的 tags）
    // → 兜底模型（models.fallbackModel）→ 网关目录首个对话模型。所以真正接线
    // 的只有这四个键 + timeout + gatewayBase。AgentHub 那套「模型来源排序 +
    // 自定义供应商 + 用途标签集」在本仓没有对应的解析器，标 unwired 而不是
    // 留一个改完没反应的输入框。
    unwired(knob_d("models.sourceOrder", Kind::List, Tier::Advanced, "模型与网关", "模型来源优先级",
        "本仓未接入：模型只从本机网关目录里取，没有「网关 / 自定义供应商」两个来源可排序", "[\"gateway\",\"custom\"]")),
    unwired(knob_d("models.providers", Kind::List, Tier::Advanced, "模型与网关", "自定义供应商",
        "本仓未接入：模型只从本机网关目录读取，这组自定义供应商（名称 / 基址 / 密钥）没有解析器", "[]")),
    knob("models.models", Kind::List, Tier::Advanced, "模型与网关", "模型池", "[]"),
    knob_d("models.routing", Kind::List, Tier::Internal, "模型与网关", "按标签降级链",
        "每项 {\"task\":\"extract\",\"tags\":[\"light\"],\"model\":\"...\"}", "[]"),
    unwired(knob_d("models.tagDefs", Kind::List, Tier::Internal, "模型与网关", "用途标签集",
        "本仓未接入：自动化任务的用途标签写在代码里的路由表，这组标签集没有解析器",
        "[\"light\",\"heavy\",\"dedup\",\"classify\",\"distill\",\"extract\",\"tag\",\"summarize\",\"profile\",\"supersede\",\"consolidate\",\"insight\"]")),
    range(knob("models.timeout", Kind::Number, Tier::Advanced, "模型与网关", "请求超时（秒）", "60"), 5.0, 600.0),
    unwired(range(knob_d("models.maxRetries", Kind::Number, Tier::Advanced, "模型与网关", "失败重试次数",
        "本仓未接入：内部模型调用失败即返回，重试由自动化任务下一轮承担；不在这里叠一层", "3"), 0.0, 10.0)),
    knob_d("models.gatewayBase", Kind::Text, Tier::Internal, "模型与网关", "网关基址",
        "留空 = 自动探测本机端口（面板与网关分端口时也能找对）", "\"\""),
    knob_d("models.fallbackModel", Kind::Text, Tier::Advanced, "模型与网关", "兜底模型",
        "路由表与模型池都没命中时用它；留空 = 取模型目录里第一个对话模型", "\"\""),
    knob("models.taskEffort", Kind::Map, Tier::Internal, "模型与网关", "各任务思考强度覆盖",
        "{\"extract\":\"low\",\"tag\":\"minimal\",\"classify\":\"minimal\",\"summarize\":\"low\",\
          \"distill\":\"medium\",\"profile\":\"high\",\"dedup\":\"low\",\"supersede\":\"low\"}"),
    // 各任务输出 token 上限。没列到的任务走 `llm::max_tokens_for` 里的默认表。
    //
    // 为什么这必须是一个可见的旋钮：`distill` / `profile` 一次要产出 3-8 条带证据
    // 的结论，1200 token 会被截断，截断后 JSON 解析失败，用户看到的是「深度记忆
    // 0 条」而不是「预算不够」。留一个能改的键，比让用户去猜 prompt 有用。
    knob("models.taskMaxTokens", Kind::Map, Tier::Internal, "模型与网关", "各任务输出 token 上限",
        "{\"extract\":2400,\"tag\":1600,\"classify\":800,\"summarize\":1600,\
          \"distill\":3000,\"profile\":3000,\"consolidate\":3000,\"supersede\":800,\
          \"dedup\":800,\"insight\":1600}"),

    // ===== 导入 =====
    knob_d("import.dryRunFirst", Kind::Bool, Tier::Advanced, "导入", "导入前必须干跑预览", "", "true"),
    range(knob("import.batchSize", Kind::Number, Tier::Internal, "导入", "每批写入条数", "1000"), 20.0, 2000.0),
    knob_d("import.sensitiveSkip", Kind::Bool, Tier::Advanced, "导入", "疑似敏感内容默认跳过", "", "true"),
    knob_d("import.autoIngest", Kind::Bool, Tier::Basic, "导入", "日常会话自动入库",
        "开着时「会话增量入库」任务会定期把本机客户端新产生的会话搬进记忆；关掉就只剩手动导入",
        "true"),
    range(knob_d("import.autoSinceDays", Kind::Number, Tier::Advanced, "导入", "首次自动入库回溯天数",
        "还没有水位时，只读最近 N 天改过的会话文件；之后按水位增量读", "7"), 1.0, 365.0),
    knob_d("import.sources", Kind::List, Tier::Advanced, "导入", "导入来源清单",
        "路径/格式/启用/优先级；Markdown 笔记目录默认关，选好路径再开", "[]"),
    knob_d("import.overrides", Kind::Map, Tier::Internal, "导入", "来源路径覆盖",
        "内置来源 id → 用户手填的路径；有值时优先于候选路径", "{}"),
    knob_d("import.ignored", Kind::List, Tier::Internal, "导入", "已忽略的来源 id",
        "用户主动关掉的内置来源；不在清单里 = 未被忽略", "[]"),

    // ===== 隐私 =====
    knob_d("privacy.redact", Kind::Bool, Tier::Advanced, "隐私", "写入前脱敏",
        "开启后，写入的正文按下面的规则逐条替换成 ***（导入与手动写入都过这一道）；规则为空时不脱敏并记一条告警",
        "false"),
    knob_d("privacy.redactRules", Kind::List, Tier::Internal, "隐私", "脱敏规则（正则）",
        "每条一个 Rust 正则，命中即整段替换为 ***；写坏了某条只跳过那条并记日志，不影响其它规则",
        "[\"sk-[A-Za-z0-9_-]{8,}\",\"Bearer\\\\s+[A-Za-z0-9._-]+\",\"AKIA[0-9A-Z]{16}\",\
          \"password\\\\s*[:=]\\\\s*\\\\S+\",\"1[3-9]\\\\d{9}\",\"\\\\b\\\\d{17}[\\\\dXx]\\\\b\"]"),
    knob_d("privacy.pause", Kind::Bool, Tier::Advanced, "隐私", "隐私模式（暂停一切采集）", "", "false"),

    // ===== WebDAV 同步 =====
    //
    // ── 为什么这里有一整组 `sync.modules.*` ──────────────────────
    // 记忆同步原本只同步「记忆」自己（general / projects / profile）。界面上
    // 要能一眼说清「同一个网盘里，谁在哪个目录」，所以每个可能上云的模块都
    // 有一个**根目录名**旋钮，默认值与 AgentHub 对齐（`/agent-skills` 这些）。
    // 目录名留空 = 该模块不参与同步 —— 这比另加一个「启用」开关少一个可能与
    // 目录名不一致的状态。
    //
    // 但**本仓只有记忆实现了同步**（见 `sync::MODULES` 里 `implemented: true` 那一条）。
    // 其余几个旋钮照实存在、照实落盘、也照实显示，只是运行时不读它们 —— 界面上
    // 明确标注「未实现」，不做一个点了没反应的「立即同步」。
    knob("sync.enabled", Kind::Bool, Tier::Basic, "同步", "同步总开关", "true"),
    opts(knob_d("sync.provider", Kind::Enum, Tier::Basic, "同步", "存储类型",
        "本仓只实现 WebDAV；S3 需要重写打包与列目录，尚未接入",
        "\"webdav\""), &["webdav", "s3"]),
    knob("sync.auto", Kind::Bool, Tier::Basic, "同步", "自动定时同步", "true"),
    knob_d("sync.url", Kind::Text, Tier::Basic, "同步", "WebDAV 地址",
        "留空 = 用默认地址（坚果云 dav.jianguoyun.com）；换别家网盘就填它给的地址", "\"\""),
    knob_d("sync.username", Kind::Text, Tier::Basic, "同步", "WebDAV 用户名",
        "留空 = 还没配好同步（同步页显示「未配置」）；少数自建服务允许匿名写时才可空", "\"\""),
    knob_d("sync.password", Kind::Secret, Tier::Basic, "同步", "WebDAV 密码",
        "经系统密钥加密后落 config.json，接口只回「已设置」；不进同步包", "\"\""),
    knob_d("sync.deviceName", Kind::Text, Tier::Advanced, "同步", "本机显示名",
        "留空 = 用系统电脑名（也读不到才退到设备 id）；随同步包上传，别的设备在设备列表里看到的就是它", "\"\""),
    knob_d("sync.remoteDir", Kind::Text, Tier::Advanced, "同步", "远端目录名",
        "同一账号下的多台设备共用这个目录", "\"agent2api-memory\""),
    opts(knob_d("sync.preset", Kind::Enum, Tier::Advanced, "同步", "存储预设",
        "仅备忘：帮你记住这台服务器是哪家的，不改变任何同步行为", "\"custom\""),
        &["custom", "jianguoyun", "nextcloud", "synology", "fnos"]),
    unwired(knob_d("sync.confirmPhrase", Kind::Secret, Tier::Advanced, "同步", "号池包加密口令",
        "本仓未接入号池同步：这个口令只被记录与回显「已设置」，不参与任何加解密", "\"\"")),
    range(knob("sync.intervalMin", Kind::Number, Tier::Advanced, "同步", "同步间隔（分钟）", "60"), 5.0, 1440.0),
    range(knob("sync.packSizeLimitMB", Kind::Number, Tier::Internal, "同步", "单包体积上限 (MB)", "50"), 1.0, 2048.0),
    knob_d("sync.excludeIndex", Kind::Bool, Tier::Internal, "同步", "索引库不入同步包",
        "索引可重建，且是二进制无法三方合并", "true"),

    // ===== 各模块同步根目录 =====
    //
    // 五个键里只有「记忆」那一行真的驱动同步；其余四个本仓没有对应的模块实现，
    // 目录名只用于界面展示（同步页要能一眼说清「同一个网盘里谁在哪个目录」）。
    // 所以那四个标 unwired：改了不会让任何东西开始同步。
    unwired(knob_d("sync.modules.skills", Kind::Text, Tier::Advanced, "同步目录", "技能仓库根目录",
        "本仓未接入技能同步：这里只决定同步页上显示的目录名，不会真的传", "\"/agent-skills\"")),
    unwired(knob_d("sync.modules.dosage", Kind::Text, Tier::Advanced, "同步目录", "用量统计根目录",
        "本仓未接入用量同步：这里只决定同步页上显示的目录名，不会真的传", "\"/dosage-sync\"")),
    unwired(knob_d("sync.modules.pool", Kind::Text, Tier::Advanced, "同步目录", "号池管家号池根目录",
        "本仓未接入号池同步：这里只决定同步页上显示的目录名，不会真的传", "\"/agenthub-proxy\"")),
    unwired(knob_d("sync.modules.memory", Kind::Text, Tier::Advanced, "同步目录", "记忆中枢根目录",
        "本仓未接入路径自定义：记忆上传目录由 `sync.remoteDir` 决定，改这里不会换目录（只影响同步页显示的目录名）",
        "\"/agenthub-memory\"")),
    unwired(knob_d("sync.modules.prompts", Kind::Text, Tier::Advanced, "同步目录", "提示词库根目录",
        "本仓未接入提示词库同步：这里只决定同步页上显示的目录名，不会真的传", "\"/agenthub-prompts\"")),

    // ===== 界面 =====
    // 这四条以前是纯死键：`ui.pageSize` 后端当成「没传 limit 时的默认值」，
    // 而前端每次都显式传 50，等于永远不生效；`ui.defaultTab` / `ui.tabs` /
    // `ui.realtimeRefresh` 前端根本不读。现在前三条由 `memory-page.tsx` 真读
    //（`ui.tabs` 还带一条硬保护：配置页签永远可见，否则用户把「配置」关掉之后
    // 就再也改不回来了）；`ui.realtimeRefresh` 本仓没有轮询，标 unwired。
    range(knob_d("ui.pageSize", Kind::Number, Tier::Basic, "界面", "列表每页条数",
        "浏览页每页条数；后端在 `limit` 缺省时也用这个值", "50"), 10.0, 500.0),
    opts(knob("ui.defaultTab", Kind::Enum, Tier::Advanced, "界面", "默认页签", "\"dashboard\""),
        &["dashboard", "browse", "projects", "profile", "agents", "auto", "import", "sync", "settings"]),
    opts(knob_d("ui.tabs", Kind::Multi, Tier::Advanced, "界面", "显示的页签",
        "只控制顶栏显示哪些页签；「配置」始终可见 —— 否则关掉它就再也改不回来",
        "[\"dashboard\",\"browse\",\"projects\",\"profile\",\"agents\",\"auto\",\"import\",\"sync\"]"),
        &["dashboard", "browse", "projects", "profile", "agents", "auto", "import", "sync", "settings"]),
    unwired(knob_d("ui.realtimeRefresh", Kind::Bool, Tier::Basic, "界面", "浏览页实时刷新",
        "本仓未接入轮询：浏览页只在写入后 / 点「刷新」时重取，没有后台定时重拉。开关打开也不会开始轮询", "true")),
];

pub fn schema() -> &'static [Knob] {
    SCHEMA
}

pub fn find(key: &str) -> Option<&'static Knob> {
    SCHEMA.iter().find(|item| item.key == key)
}

/// 默认值（解析一次缓存）。表里每个键都保证能解析 —— 解析不了就是这里写错了，
/// 单元测试 `every_default_parses` 把这条钉住。
pub fn default_of(key: &str) -> Option<Value> {
    let item = find(key)?;
    serde_json::from_str::<Value>(item.default_json).ok()
}

// ── 存取 ─────────────────────────────────────────────────────

/// 模块总开关。与其余旋钮分开存：它决定「记忆服务是否对外提供工具」，
/// 是唯一一个即使在配置读不出来时也要有确定答案的键。
pub const ENABLE_KEY: &str = "enabled";

/// 配置文件的内存表示：`enabled` + 只包含**显式设置过**的扁平键。
#[derive(Debug, Default, Clone)]
pub struct Stored {
    pub enabled: bool,
    pub values: BTreeMap<String, Value>,
}

impl Stored {
    /// 取一个列表键的字符串项 —— **读的是这份内存状态**，不是磁盘、不补默认值。
    ///
    /// 为什么不用 [`strings_of`]：那个函数按 mtime 走缓存，在 [`update`] 的
    /// 闭包里「刚改完再读」会拿到改之前的值。写入口要的是「我手上这一份现在
    /// 是什么」，所以给它一个只认 `self` 的读取器。
    pub fn strings(&self, key: &str) -> Vec<String> {
        self.values
            .get(key)
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    }

    /// 取一个对象键 —— 同样只认这份内存状态（理由见 [`Stored::strings`]）。
    pub fn map_of(&self, key: &str) -> Map<String, Value> {
        self.values
            .get(key)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    }

    /// 整份替换一个键（校验后写入，**不做深合并**）。
    ///
    /// 为什么需要它：`patch` 对 `Kind::Map` 走的是深合并，而深合并**删不掉键**。
    /// 「改路径为空 = 删掉那条覆盖」这类动作用 `patch` 是无效的 —— 传进去的对象
    /// 里少了那个键，合并时旧值还在，用户看到的是「删了但没删掉」。这里给一个
    /// 明确的「整份替换」语义，校验规则仍走同一个 `coerce`。
    pub fn set(&mut self, key: &str, value: Value) -> Result<(), String> {
        if key == ENABLE_KEY {
            self.enabled = value.as_bool().ok_or("enabled 必须是布尔值")?;
            return Ok(());
        }
        let Some(knob) = find(key) else {
            // 未知键静默丢弃，与 `apply_one` 同口径
            return Ok(());
        };
        // 秘密字段拒绝走这条路：`coerce` 只校验类型、不加密，用它写进去就是
        // 明文口令躺在 config.json 里。加密那段逻辑在 `apply_one`（它还要处理
        // 「空串 = 沿用」），这里没有旧值可比，也就没法正确实现 —— 明确报错，
        // 比悄悄落一份明文安全。
        if knob.kind == Kind::Secret {
            return Err(format!("「{}」是秘密字段，用 patch 写入（set 不做加密）", knob.label));
        }
        let fixed = coerce(knob, &value)?;
        self.values.insert(key.to_string(), fixed);
        Ok(())
    }

    fn from_json(value: &Value) -> Self {
        let enabled = value.get(ENABLE_KEY).and_then(Value::as_bool).unwrap_or(true);
        let mut values = BTreeMap::new();
        if let Some(map) = value.get("values").and_then(Value::as_object) {
            for (key, item) in map {
                values.insert(key.clone(), item.clone());
            }
        }
        // 老形状（v1 的 camelCase 顶层键）就地折进扁平表：
        // `coreMaxTokens` → `agents.coreMaxTokens`。读一次就迁完，不写盘。
        if let Some(tokens) = value.get("coreMaxTokens").and_then(Value::as_u64) {
            values.entry("agents.coreMaxTokens".to_string()).or_insert(json!(tokens));
        }
        Stored { enabled, values }
    }

    fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert(ENABLE_KEY.to_string(), json!(self.enabled));
        let mut values = Map::new();
        for (key, item) in &self.values {
            values.insert(key.clone(), item.clone());
        }
        map.insert("values".to_string(), Value::Object(values));
        Value::Object(map)
    }
}

/// `config.json` 的**读改写锁**。
///
/// ── 为什么必须有 ────────────────────────────────────────────
/// [`patch`] / [`reset`] / [`replace`] 三个写入口的形状完全一样：**读整份
/// 配置 → 改几个键 → 整份写回**。并发时后写的那一份整份覆盖前一份，前一份
/// 改的键静默消失 —— 文件层不报错、页面显示「已保存」、值就是没了。
///
/// 这不是理论风险：导入页的 `enable` 会连发两次 `patch`（`import.sources`
/// 一次、`import.ignored` 一次），页面保存与 MCP 调用也在同一时刻发生；两个
/// 用户在两个标签页里各改一个旋钮就是稳定的丢数据现场。
///
/// ── 为什么锁在这里而不是在 `write_atomic` ────────────────────
/// `write_atomic` 只保证「一个文件不会写出半个」，它不知道这次写是「基于哪
/// 一份读的」。原子写 + 丢失更新 = 一条完整但错误的新配置。锁必须包住
/// 「读」和「写」两端。
///
/// ── 与其它锁的获取顺序 ──────────────────────────────────────
/// 与 `store` 的两把锁（[`store::lock_writes`] / `store::lock_projects`）完全
/// 独立，也没有任何路径会「持本锁去取 store 那两把」或反过来 —— 所以与它们
/// 之间不存在环。
///
/// 唯一会嵌套的是缓存锁 [`slot`]：本锁 → `commit` → `write_atomic` →
/// `storage.backupBeforeWrite` → [`stored`] → `slot()`。方向固定为「本锁 → slot」，
/// 而 `slot()` 内部只读文件、从不反向取本锁，因此同样不成环。**但顺序不能反**：
/// 先持 `slot()` 再落盘，`stored()` 里的 `slot().lock()` 就会自锁死（std 的
/// `Mutex` 不可重入）。`commit` 因此是「先写盘、后刷缓存」。
static CONFIG_LOCK: Mutex<()> = Mutex::new(());

fn lock_config() -> std::sync::MutexGuard<'static, ()> {
    CONFIG_LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// 配置缓存的唯一句柄（mtime + 内容）。
fn slot() -> &'static Mutex<(Option<SystemTime>, Stored)> {
    static CACHE: OnceLock<Mutex<(Option<SystemTime>, Stored)>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new((None, Stored { enabled: true, values: BTreeMap::new() })))
}

/// 读配置。按 mtime 缓存：这个函数在 MCP 工具调用与页面每次刷新都会走到，
/// 每次都真读文件是纯浪费；改配置后下一次调用就能生效，不必重启进程。
pub fn stored() -> Stored {
    let path = store::config_file();
    let stamp = std::fs::metadata(&path).ok().and_then(|meta| meta.modified().ok());
    let Ok(mut guard) = slot().lock() else {
        return Stored { enabled: true, values: BTreeMap::new() };
    };
    if guard.0 == stamp {
        return guard.1.clone();
    }
    let loaded = read_file(&path);
    *guard = (stamp, loaded.clone());
    loaded
}

/// 从磁盘真读一份，并把缓存同步成这一份（写入口专用）。
///
/// 为什么不直接用 [`stored`]：它按 mtime 判新鲜度，而 Windows 的 mtime 精度
/// 与「同一秒内两次写」会撞在一起 —— 恰好在这一秒里做读改写，就会拿到上一版
/// 内容再把这一版写回去。写入口不信缓存，直接读盘。
fn read_disk() -> Stored {
    let path = store::config_file();
    let stamp = std::fs::metadata(&path).ok().and_then(|meta| meta.modified().ok());
    let loaded = read_file(&path);
    let mut guard = slot().lock().unwrap_or_else(|poison| poison.into_inner());
    *guard = (stamp, loaded.clone());
    loaded
}

fn read_file(path: &Path) -> Stored {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .map(|value| Stored::from_json(&value))
        .unwrap_or(Stored { enabled: true, values: BTreeMap::new() })
}

/// 落盘 + 把缓存刷成刚写的这一份。
///
/// 顺序不能反：`write_atomic` 内部要读 `storage.backupBeforeWrite`，那会走
/// [`stored`] 取缓存锁 —— 若先持缓存锁再写盘就是自锁死（std 的 Mutex 不可重入）。
fn commit(next: &Stored) -> Result<(), String> {
    store::ensure_layout()?;
    let text = serde_json::to_string_pretty(&next.to_json()).map_err(|error| error.to_string())?;
    let path = store::config_file();
    store::write_atomic(&path, &text)?;
    let stamp = std::fs::metadata(&path).ok().and_then(|meta| meta.modified().ok());
    let mut guard = slot().lock().unwrap_or_else(|poison| poison.into_inner());
    *guard = (stamp, next.clone());
    Ok(())
}

/// 取一个旋钮的生效值（显式值优先，否则表里的默认值）。
pub fn get(key: &str) -> Option<Value> {
    let cache = stored();
    if key == ENABLE_KEY {
        return Some(json!(cache.enabled));
    }
    match cache.values.get(key) {
        Some(value) => Some(value.clone()),
        None => default_of(key),
    }
}

/// 取数字。类型不对时回落到默认值 —— 调用方不该因为一个坏配置就崩。
pub fn number(key: &str) -> f64 {
    get(key)
        .and_then(|value| value.as_f64().or_else(|| value.as_i64().map(|raw| raw as f64)))
        .or_else(|| default_of(key).and_then(|value| value.as_f64()))
        .unwrap_or(0.0)
}

pub fn usize_of(key: &str) -> usize {
    let value = number(key);
    if value.is_finite() && value > 0.0 { value as usize } else { 0 }
}

pub fn bool_of(key: &str) -> bool {
    get(key)
        .and_then(|value| value.as_bool())
        .or_else(|| default_of(key).and_then(|value| value.as_bool()))
        .unwrap_or(false)
}

pub fn text_of(key: &str) -> String {
    // 秘密字段要从密文里解，普通文本直接取。走同一个入口是为了让调用方不用
    // 关心「这个键是不是加密存的」—— `sync::auth_headers` 只想要那个密码。
    if find(key).map(|item| item.kind) == Some(Kind::Secret) {
        return secret_of(key);
    }
    get(key).and_then(|value| value.as_str().map(str::to_string)).unwrap_or_default()
}

pub fn strings_of(key: &str) -> Vec<String> {
    get(key)
        .and_then(|value| value.as_array().cloned())
        .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

pub fn object_of(key: &str) -> Map<String, Value> {
    get(key).and_then(|value| value.as_object().cloned()).unwrap_or_default()
}

/// 整份生效配置（扁平键 → 值）。页面拿它渲染表单。
///
/// ── 秘密字段在这里被替换成空串 ──────────────────────────────
/// 这是**唯一**的下发给页面的入口（`GET/PUT /api/memory/config` 都走它），
/// 所以脱敏放在这里一处就够：`Secret` 类的值只回 `""`，页面据此知道「这个
/// 输入框现在是空的」，另有 `secret_set` 告诉它「库里有没有值」。
///
/// 不要把密文原样发下去再让前端「不许显示」—— 接口能被 curl，那条约束只在
/// 浏览器里有意义。真正要用的地方（`sync::auth_headers`）走
/// [`secret_of`] 自己解。
pub fn flat() -> BTreeMap<String, Value> {
    let cache = stored();
    let mut out = BTreeMap::new();
    for item in SCHEMA {
        let value = cache
            .values
            .get(item.key)
            .cloned()
            .or_else(|| default_of(item.key))
            .unwrap_or(Value::Null);
        let value = if item.kind == Kind::Secret && !value.as_str().unwrap_or("").is_empty() {
            // 有值就回一个空串，语义是「别回显，但你知道我配过」
            json!("")
        } else {
            value
        };
        out.insert(item.key.to_string(), value);
    }
    out
}

/// 秘密字段是否已在磁盘上配置过（给页面显示「已保存口令」）。
pub fn secret_set(key: &str) -> bool {
    // `stored()` 每次返回的是**克隆**（`Stored` 里有 `BTreeMap`），绑到局部变量
    // 才能借用里面的字符串。写成 `.and_then(|_| stored().values.get(key))` 会
    // 得到 `E0515`：临时值在语句结束就被丢弃，返回的引用悬空。
    let cache = stored();
    find(key)
        .filter(|item| item.kind == Kind::Secret)
        .and_then(|_| cache.values.get(key))
        .and_then(Value::as_str)
        .map(|text| !text.trim().is_empty())
        .unwrap_or(false)
}

/// 解出秘密字段的明文（仅供后端自用，**不要**放进任何 JSON 响应）。
///
/// 三种情况都回落空串而不是报错：
///   · 没配过（值就是空）；
///   · 值是明文（老版本 / 手工编辑 `config.json` 写进去的）；
///   · 密文解开失败（密钥文件被换过）—— 此时当作没配，让用户重填一次，
///     比抛一个「解密失败」把整条同步流程卡死要好。
pub fn secret_of(key: &str) -> String {
    // 同上：先把克隆落到局部变量，`value` 借用的是它，不是临时值。
    let cache = stored();
    let Some(value) = cache.values.get(key).and_then(Value::as_str) else {
        return String::new();
    };
    let value = value.trim();
    if value.is_empty() {
        return String::new();
    }
    if !crate::server::core::crypto::is_envelope(value) {
        return value.to_string();
    }
    crate::server::core::crypto::open(value, crate::server::core::crypto::SecretScope::MemorySync)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_default()
}

/// 只取显式设置过的键（导出/同步用：不把默认值写死在别人机器上）。
pub fn explicit() -> BTreeMap<String, Value> {
    stored().values
}

/// 整份配置的嵌套树（`a.b` → `{a:{b:…}}`）。给人看的形状。
pub fn tree() -> Value {
    let mut root = Map::new();
    for (key, value) in flat() {
        let segments: Vec<&str> = key.split('.').collect();
        let mut cursor = &mut root;
        for segment in &segments[..segments.len() - 1] {
            let entry = cursor.entry(segment.to_string()).or_insert_with(|| Value::Object(Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(Map::new());
            }
            cursor = entry.as_object_mut().expect("刚确认是 object");
        }
        cursor.insert(segments[segments.len() - 1].to_string(), value);
    }
    Value::Object(root)
}

/// 校验一个值是否符合旋钮定义，顺手做类型归一（字符串数字 → 数字等）。
pub fn coerce(knob: &Knob, value: &Value) -> Result<Value, String> {
    let label = knob.label;
    match knob.kind {
        Kind::Number => {
            let raw = match value {
                Value::Number(number) => number.as_f64().unwrap_or(0.0),
                Value::String(text) => text.trim().parse::<f64>().map_err(|_| format!("「{label}」必须是数字"))?,
                _ => return Err(format!("「{label}」必须是数字")),
            };
            if !raw.is_finite() {
                return Err(format!("「{label}」必须是有限数字"));
            }
            let mut clamped = raw;
            if let Some(min) = knob.min {
                clamped = clamped.max(min);
            }
            if let Some(max) = knob.max {
                clamped = clamped.min(max);
            }
            // 整数值用整数存：JSON 里 3 比 3.0 干净，比较也不会因浮点抖动出错
            if clamped.fract() == 0.0 && clamped.abs() < 9e15 {
                Ok(json!(clamped as i64))
            } else {
                Ok(json!(clamped))
            }
        }
        Kind::Bool => value
            .as_bool()
            .map(|flag| json!(flag))
            .ok_or_else(|| format!("「{label}」必须是布尔值")),
        Kind::Text | Kind::Path => value
            .as_str()
            .map(|text| json!(text))
            .ok_or_else(|| format!("「{label}」必须是字符串")),
        // 加密在 `apply_one` 里做（那里要判断「空串 = 沿用」，coerce 看不到旧值）。
        // 这里只做类型校验，免得 `coerce` 单独被调用时把明文当合法值放行。
        Kind::Secret => value
            .as_str()
            .map(|text| json!(text))
            .ok_or_else(|| format!("「{label}」必须是字符串")),
        Kind::Enum => {
            let text = value.as_str().ok_or_else(|| format!("「{label}」必须是字符串"))?;
            if knob.options.contains(&text) {
                Ok(json!(text))
            } else {
                Err(format!("「{label}」必须是 {} 之一", knob.options.join(" / ")))
            }
        }
        Kind::Multi => {
            let items = value.as_array().ok_or_else(|| format!("「{label}」必须是数组"))?;
            let mut out: Vec<String> = Vec::new();
            for item in items {
                let text = item.as_str().ok_or_else(|| format!("「{label}」的每一项都必须是字符串"))?;
                if !knob.options.is_empty() && !knob.options.contains(&text) {
                    return Err(format!("「{label}」含未知选项：{text}"));
                }
                if !out.iter().any(|seen| seen == text) {
                    out.push(text.to_string());
                }
            }
            Ok(json!(out))
        }
        Kind::List => {
            let items = value.as_array().ok_or_else(|| format!("「{label}」必须是数组"))?;
            Ok(Value::Array(items.clone()))
        }
        Kind::Map => {
            if value.is_object() {
                Ok(value.clone())
            } else {
                Err(format!("「{label}」必须是对象"))
            }
        }
    }
}

/// 部分更新：只改传进来的键。返回更新后的嵌套树。
///
/// 接受的形状：
/// * `{"agents.coreMaxTokens": 900}` —— 扁平点号键（页面与同步用）
/// * `{"enabled": false}` —— 总开关
/// * `{"coreMaxTokens": 900}` —— v1 的 camelCase 旧键（自动映射）
///
/// **整段持 [`CONFIG_LOCK`]**：这是「读整份 → 改几个键 → 整份写回」，两个
/// 并发调用各改一个键时，后写的会把前一个键抹掉（见锁上的说明）。
pub fn patch(incoming: &Value) -> Result<Value, String> {
    // 直接把 `update` 的结果返回：它落盘后已经算过一次嵌套树，再调一次 `tree()`
    // 等于多读一遍缓存 —— 而且那两棵树之间理论上还可能被别的写者插进来改一次，
    // 返回给调用方的就不再是「我刚写进去的那一份」了。
    update(|next| apply_all(next, incoming))
}

/// 在一个**写事务**里改配置：闭包拿到「刚从磁盘读上来的那一份」，改完由本
/// 函数整份落盘。成功返回落盘后的嵌套树。
///
/// ── 为什么要有它，而不只有 [`patch`] ────────────────────────
/// 有些业务动作要改两个键且必须**同生共死**：导入页「启用一个内置来源」是
/// 「`import.sources` 加一条 + `import.ignored` 摘掉它」。拆成两次 `patch`，
/// 中间任何一次失败（或另一路并发写入插进来）都会留下「清单里有、忽略名单里
/// 也还有」的自相矛盾状态 —— 页面显示「已启用」，扫描却仍然跳过它。
/// 闭包形态让「读到的这一份」在两次修改之间保持同一，中途不会被别人插队。
///
/// ── 闭包里必须用 [`Stored::strings`] 读 ──────────────────────
/// 不能用模块级的 [`strings_of`]：那个按 mtime 走缓存，闭包里读到的是「改之前」
/// 的值，于是第二次修改会把第一次的结果覆盖掉（正是要防的那类 bug）。
pub fn update(edit: impl FnOnce(&mut Stored) -> Result<(), String>) -> Result<Value, String> {
    let _guard = lock_config();
    let mut next = read_disk();
    edit(&mut next)?;
    commit(&next)?;
    Ok(tree())
}

/// 把一份 `patch` 形状的 JSON 全部应用到 `next` 上（[`patch`] 与 [`update`]
/// 共用）。**调用方必须已持有 [`CONFIG_LOCK`]** —— 它只改内存，不落盘。
fn apply_all(next: &mut Stored, incoming: &Value) -> Result<(), String> {
    let object = incoming.as_object().ok_or("配置必须是对象")?;
    for (raw_key, value) in object {
        // 页面可能把整个嵌套树发回来（`{storage:{root:""}}`）：展开成扁平键再逐条校验。
        if find(raw_key).is_none() && value.is_object() {
            for (sub_key, sub_value) in value.as_object().expect("已确认 object") {
                let dotted = format!("{raw_key}.{sub_key}");
                apply_one(next, &dotted, sub_value)?;
            }
            continue;
        }
        apply_one(next, raw_key, value)?;
    }
    Ok(())
}

/// 在 [`update`] 的事务里应用一份 `patch` 形状的 JSON（给「顺手再改一个键」用）。
pub fn apply_in(next: &mut Stored, incoming: &Value) -> Result<(), String> {
    apply_all(next, incoming)
}

fn apply_one(next: &mut Stored, key: &str, value: &Value) -> Result<(), String> {
    if key == ENABLE_KEY {
        next.enabled = value.as_bool().ok_or("enabled 必须是布尔值")?;
        return Ok(());
    }
    // v1 旧键映射
    let mapped = match key {
        "coreMaxTokens" => "agents.coreMaxTokens".to_string(),
        other => other.to_string(),
    };
    let Some(knob) = find(&mapped) else {
        // 未知键静默丢弃：老版本配置里有、新版本删掉的键不该让整份配置保存失败
        return Ok(());
    };
    if knob.kind == Kind::Secret {
        // 秘密字段的空串 = **沿用库里那份**（与 `cloud_sync::settings` 同语义）。
        // 界面拿到的是「已设置」而不是原文，所以它每次保存必然发空串 —— 若把
        // 空串当成「清空」，用户改一个不相干的旋钮就会把密码抹掉。
        // 想真正清空用 `reset`（那里走的是把键从 values 里删掉）。
        let incoming = value.as_str().ok_or_else(|| format!("「{}」必须是字符串", knob.label))?;
        if incoming.is_empty() {
            return Ok(());
        }
        let sealed = crate::server::core::crypto::seal(
            incoming.as_bytes(),
            crate::server::core::crypto::SecretScope::MemorySync,
        )?;
        next.values.insert(mapped, json!(sealed));
        return Ok(());
    }
    if knob.kind == Kind::Map || knob.kind == Kind::List {
        // 对象/列表类做**深合并**：`auto.tasks` 只发改动的那一个任务时，
        // 其余任务的配置必须保留（整体替换会把用户其他任务的开关抹掉）。
        //
        // 基准取 `next` 里当前那一份，**不取 `get()`**：`get()` 走 mtime 缓存，
        // 在事务里（同一把锁内连改两次同一个 Map）第二次会合并到改之前的旧值，
        // 第一次改的键当场丢掉。`next` 就是本次事务正在编辑的那一份。
        let current = next
            .values
            .get(&mapped)
            .cloned()
            .or_else(|| default_of(&mapped))
            .unwrap_or(Value::Null);
        let merged = merge(current, knob, value.clone())?;
        next.values.insert(mapped, merged);
        return Ok(());
    }
    let coerced = coerce(knob, value)?;
    next.values.insert(mapped, coerced);
    Ok(())
}

fn merge(current: Value, knob: &Knob, incoming: Value) -> Result<Value, String> {
    let coerced = coerce(knob, &incoming)?;
    let (Value::Object(mut base), Value::Object(over)) = (current, coerced.clone()) else {
        return Ok(coerced);
    };
    for (key, value) in over {
        if value.is_object() {
            let existing = base.get(&key).cloned().unwrap_or_else(|| Value::Object(Map::new()));
            base.insert(key, deep_merge(existing, value));
        } else {
            base.insert(key, value);
        }
    }
    Ok(Value::Object(base))
}

fn deep_merge(base: Value, over: Value) -> Value {
    // 用 match 而不是 let-else：let-else 会把被匹配的值移走，else 分支里
    // 就再也拿不到 `over`（而「类型不是对象就整体覆盖」正是这里要的语义）
    match (base, over) {
        (Value::Object(mut base_map), Value::Object(over_map)) => {
            for (key, value) in over_map {
                // `remove` 而不是 `get().cloned()`：递归时要那层的所有权，克隆一份
                // 再丢掉原值等于每层多一次深拷贝，任务表嵌三层时是可见的开销
                let merged = match base_map.remove(&key) {
                    Some(Value::Object(inner)) if value.is_object() => {
                        deep_merge(Value::Object(inner), value)
                    }
                    _ => value,
                };
                base_map.insert(key, merged);
            }
            Value::Object(base_map)
        }
        (_, over) => over,
    }
}

/// 整份替换（导入配置用）。`enabled` 缺省为 true。
pub fn replace(value: &Value) -> Result<Value, String> {
    update(|next| {
        let parsed = Stored::from_json(value);
        let mut clean: BTreeMap<String, Value> = BTreeMap::new();
        // 逐键过一遍校验，把非法值挡在落盘之前；未知键直接丢（老配置兼容）
        for (key, current) in parsed.values {
            let Some(knob) = find(&key) else { continue };
            clean.insert(key, coerce(knob, &current)?);
        }
        // 整份替换 = 旧值全清再灌新的。不做这一步，「导入一份配置」就变成了
        // 「把这份配置合并进来」—— 上一份里有、这一份里没有的键会静默残留。
        next.values = clean;
        next.enabled = parsed.enabled;
        Ok(())
    })
}

/// 恢复默认：`keys` 为空 = 全部恢复。
pub fn reset(keys: &[String]) -> Result<Value, String> {
    let keys = keys.to_vec();
    update(|next| {
        if keys.is_empty() {
            next.values.clear();
        } else {
            for key in &keys {
                next.values.remove(key);
                if let Some(mapped) = (key.as_str() == "coreMaxTokens").then(|| "agents.coreMaxTokens".to_string()) {
                    next.values.remove(&mapped);
                }
            }
        }
        Ok(())
    })
}

/// 页面用的 schema 视图（不带 Rust 枚举，全是字符串/数字，前端直接用）。
pub fn schema_json() -> Value {
    Value::Array(
        SCHEMA
            .iter()
            .map(|item| {
                json!({
                    "key": item.key,
                    "kind": match item.kind {
                        Kind::Number => "number",
                        Kind::Bool => "boolean",
                        Kind::Text => "string",
                        Kind::Path => "path",
                        Kind::Secret => "secret",
                        Kind::Enum => "enum",
                        Kind::Multi => "multiselect",
                        Kind::List => "list",
                        Kind::Map => "map",
                    },
                    "tier": item.tier.as_str(),
                    "group": item.group,
                    "label": item.label,
                    "desc": item.desc,
                    "default": default_of(item.key).unwrap_or(Value::Null),
                    "min": item.min,
                    "max": item.max,
                    "options": item.options,
                    "hot": item.hot,
                    // 运行时代码有没有真的读它。`false` = 只落盘、只显示，
                    // 前端据此把控件置灰并挂「本仓未接入」徽标 —— 不让用户
                    // 对着一个改完没反应的输入框猜自己是不是操作错了。
                    "wired": item.wired,
                    // 只有秘密字段有：库里有没有值。页面据此把占位符写成
                    // 「已保存口令，输入任意字符即进入修改」而不是「未设置」。
                    "set": if item.kind == Kind::Secret { Some(secret_set(item.key)) } else { None },
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_default_parses() {
        for item in schema() {
            assert!(
                serde_json::from_str::<Value>(item.default_json).is_ok(),
                "{} 的默认值不是合法 JSON：{}",
                item.key,
                item.default_json
            );
        }
    }

    #[test]
    fn every_default_passes_its_own_validation() {
        // 默认值必须能通过自己的校验 —— 否则「用户什么都没改」都会报错，
        // 且 reset 之后立刻变成非法状态
        for item in schema() {
            let default = default_of(item.key).expect("默认值可解析");
            if let Err(error) = coerce(item, &default) {
                panic!("{} 的默认值过不了自己的校验：{error}", item.key);
            }
        }
    }

    #[test]
    fn every_key_is_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for item in schema() {
            assert!(seen.insert(item.key), "键重复：{}", item.key);
        }
    }

    #[test]
    fn number_is_clamped_to_range() {
        let knob = find("search.finalTopK").expect("存在");
        let value = coerce(knob, &json!(999)).expect("能归一");
        assert_eq!(value, json!(50));
        let value = coerce(knob, &json!(0)).expect("能归一");
        assert_eq!(value, json!(1));
    }

    #[test]
    fn enum_rejects_unknown_option() {
        let knob = find("auto.overBudgetAction").expect("存在");
        assert!(coerce(knob, &json!("nope")).is_err());
        assert_eq!(coerce(knob, &json!("pause")).unwrap(), json!("pause"));
    }

    /// 日 token 上限默认 **0 = 不限**：预算必须是可选项，不能是默认行为。
    ///
    /// 原来默认 200000，于是每个跑过自动化的人在仪表盘上都会看到一枚
    /// 「今日预算已用完」—— 他没要求过这个限制，界面上也看不出能关。
    /// 默认值就是「产品替用户做的那个决定」：既然没人要求，就别默认拦。
    #[test]
    fn daily_token_budget_defaults_to_unlimited() {
        let knob = find("auto.dailyTokenLimit").expect("存在");
        assert_eq!(knob.default_json, "0");
        // 0 必须仍在允许范围内：否则用户填 0 会被 `coerce` 夹回下限，
        // 「不限」就成了一个填不进去的值 —— 而这是这个旋钮唯一该有的用法之一。
        assert_eq!(knob.min, Some(0.0));
        assert_eq!(coerce(knob, &json!(0)).unwrap(), json!(0));
    }

    #[test]
    fn legacy_camel_case_key_migrates() {
        let stored = Stored::from_json(&json!({ "enabled": true, "coreMaxTokens": 900 }));
        assert_eq!(stored.values.get("agents.coreMaxTokens"), Some(&json!(900)));
    }

    #[test]
    fn unknown_keys_are_dropped_not_fatal() {
        let stored = Stored::from_json(&json!({
            "enabled": true,
            "values": { "injectEnabled": true, "search.finalTopK": 12 }
        }));
        // from_json 不做过滤（那是 replace 的事），但 flat() 只按 schema 产出，
        // 所以未知键永远进不了生效配置
        let mut next = stored;
        next.values.retain(|key, _| find(key).is_some());
        assert_eq!(next.values.len(), 1);
    }

    #[test]
    fn tree_groups_dotted_keys() {
        let mut values = BTreeMap::new();
        values.insert("search.finalTopK".to_string(), json!(8));
        let mut root = Map::new();
        for (key, value) in values {
            let segments: Vec<&str> = key.split('.').collect();
            let mut cursor = &mut root;
            for segment in &segments[..segments.len() - 1] {
                let entry = cursor.entry(segment.to_string()).or_insert_with(|| Value::Object(Map::new()));
                cursor = entry.as_object_mut().unwrap();
            }
            cursor.insert(segments[segments.len() - 1].to_string(), value);
        }
        let tree = Value::Object(root);
        assert_eq!(tree.pointer("/search/finalTopK"), Some(&json!(8)));
    }

    #[test]
    fn deep_merge_keeps_untouched_siblings() {
        let base = json!({ "extract": { "enabled": true, "intervalMin": 30 } });
        let over = json!({ "extract": { "enabled": false } });
        let merged = deep_merge(base, over);
        assert_eq!(merged.pointer("/extract/enabled"), Some(&json!(false)));
        assert_eq!(merged.pointer("/extract/intervalMin"), Some(&json!(30)));
    }

    #[test]
    fn secret_knobs_are_marked_secret_and_expose_set_flag() {
        // 前端凭 `kind === 'secret'` 决定渲染密码框、凭 `set` 决定占位符；
        // 两个字段少一个，界面就会把一个秘密当普通文本回显出来。
        let schema = schema_json();
        let rows = schema.as_array().expect("schema 是数组");
        for key in ["sync.password", "sync.confirmPhrase"] {
            let row = rows
                .iter()
                .find(|item| item["key"] == json!(key))
                .unwrap_or_else(|| panic!("schema 里没有 {key}"));
            assert_eq!(row["kind"], json!("secret"), "{key} 必须是 secret 类");
            assert!(row["set"].is_boolean(), "{key} 必须下发 set 标记");
        }
        // 密码不是 path、账号不是 secret：这两处错一位就会「把账号当密码掩掉」
        // 或「把密码当路径给目录选择器」。
        let password = find("sync.password").expect("存在");
        assert_eq!(password.kind, Kind::Secret);
        let username = find("sync.username").expect("存在");
        assert_eq!(username.kind, Kind::Text);
    }

    #[test]
    fn secret_rejects_non_string_and_accepts_plain_text() {
        // `coerce` 只做类型校验（加密在 `apply_one` 里，因为那里才知道「空串 = 沿用」）。
        let knob = find("sync.password").expect("存在");
        assert!(coerce(knob, &json!(42)).is_err());
        assert_eq!(coerce(knob, &json!("s3cret")).unwrap(), json!("s3cret"));
        assert_eq!(coerce(knob, &json!("")).unwrap(), json!(""));
    }

    #[test]
    fn sync_preset_options_cover_the_frontend_list() {
        // 前端 `PRESETS` 是手写的（`memory-tab-sync.tsx`），后端是 Enum：两边
        // 一旦不一致，用户选中某个预设点保存就会拿到「不在可选范围内」的报错。
        let knob = find("sync.preset").expect("存在");
        for preset in ["custom", "jianguoyun", "nextcloud", "synology", "fnos"] {
            assert!(knob.options.contains(&preset), "预设缺 {preset}");
        }
    }

    #[test]
    fn every_module_has_a_directory_knob() {
        // 页面「模块根目录」按 `sync::MODULES` 渲染，而那一组旋钮必须真实存在：
        // 少一个键，保存时 `apply_one` 会把它当未知键静默丢弃，用户改完没反应。
        for key in [
            "sync.modules.skills",
            "sync.modules.dosage",
            "sync.modules.pool",
            "sync.modules.memory",
            "sync.modules.prompts",
        ] {
            assert!(find(key).is_some(), "缺旋钮 {key}");
        }
    }

    /// 并发写配置时，每一处改动都必须留下来。
    ///
    /// 这是 [`CONFIG_LOCK`] 的回归测试。`patch` 是「读整份 → 改 → 整份写回」，
    /// 没有锁时 20 个线程各改一个键，最后只剩最后写的那一个（**静默丢失**，
    /// 页面还会显示「已保存」）。
    ///
    /// 为什么改 `import.overrides` 而不是二十个不同的旋钮：它是个 Map，`merge`
    /// 会深合并兄弟键 —— 一把锁内 20 次写应当得到 20 个键；没有锁时后写的
    /// 整份覆盖前写的，能稳定测出「只剩一两个」。
    ///
    /// 必须 `isolated`：`cfg(test)` 下配置目录是 thread_local 的，不隔离的话
    /// 20 个线程各写各的临时目录，测试永远通过而测不到任何并发。
    #[test]
    fn concurrent_patches_keep_every_key() {
        crate::test_support::isolated(
            concat!(module_path!(), "::concurrent_patches_keep_every_key"),
            || {
                let mut handles = Vec::new();
                for index in 0..20 {
                    handles.push(std::thread::spawn(move || {
                        // 键是算出来的，所以不能用 `json!` 的字面量键形态：显式建
                        // 一个 `Map` 再包成 `Value::Object`，语义一样、没有歧义
                        let mut overrides = Map::new();
                        overrides.insert(format!("k{index}"), json!(format!("v{index}")));
                        let payload = json!({ "import.overrides": Value::Object(overrides) });
                        patch(&payload).expect("写入应当成功");
                    }));
                }
                for handle in handles {
                    handle.join().expect("线程不应 panic");
                }
                let saved = object_of("import.overrides");
                assert_eq!(
                    saved.len(),
                    20,
                    "并发写配置丢了键：实际只剩 {} 个 —— {}",
                    saved.len(),
                    serde_json::to_string(&saved).unwrap_or_default()
                );
            },
        );
    }

    /// 每条 `wired=false` 的旋钮，说明里必须自己解释清楚。
    ///
    /// ── 这条测试防的是什么 ────────────────────────────────────
    /// 把键标成「未接入」很容易，但界面上那句「本仓未接入」如果不带原因，
    /// 用户看到的就是一个**没有出口的死胡同**：它为什么没接？我还要不要配？
    /// 有没有等价操作？所以要求 `desc` 至少有一点实质长度，并且必须出现
    /// 「本仓未接入」这句话 —— 前端徽标用的是同一个词，两边对得上。
    ///
    /// 长度门槛不用很高（24 字符）：只拦「空说明」和「就写个 not implemented」，
    /// 不强迫每条都写小作文。
    #[test]
    fn unwired_knobs_explain_themselves() {
        for item in schema() {
            if item.wired {
                continue;
            }
            assert!(
                item.desc.contains("本仓未接入"),
                "{} 标了 wired=false，但说明里没有「本仓未接入」：这会让界面上的徽标变成没头没尾的一句话",
                item.key
            );
            assert!(
                item.desc.chars().count() >= 24,
                "{} 的说明太短（{} 字），没讲清楚「为什么没有」或「等价操作是什么」",
                item.key,
                item.desc.chars().count()
            );
        }
    }

    /// 每条 `wired=true` 的旋钮，本仓源码里必须真有读取点。
    ///
    /// ── 为什么把源码扫描写进单测 ───────────────────────────────
    /// 「加了旋钮但忘了读取」是这类 schema 驱动的配置最典型的 bug，而且它
    /// **不会让任何测试失败** —— 配置落盘正常、界面渲染正常，只有用户改了
    /// 发现没效果。人工 review 一百多个键看不出来，扫描源码能。
    ///
    /// 判据：在 `memory/`、`tool_services/`、`api/` 的 `.rs` 里找完整字面量
    /// `"<key>"` / `'<key>'`，且不在 `config.rs` 自身（schema 定义当然含全部键）。
    /// 松一点的判据（只找键的后半段，如 `"finalTopK"`）会让漏接的键被同名
    /// 变量蒙混过关，所以必须是带命名空间的完整串。
    ///
    /// ── 为什么前端读取点走 `NON_RUST_READERS` 而不是 `include_str!` ──
    /// `include_str!` 在**编译期**把文件读进二进制：Docker 构建只 COPY
    /// `server/src` 与 `ui/`（见 Dockerfile），不 COPY `ui-islands/` —— 用
    /// `include_str!` 指向前端源码会让镜像构建直接失败。所以前端读取点记成
    /// 「键 → 仓库内相对路径」，测试运行时用 `fs` 去读；文件不在（例如只
    /// 打了服务端包的场景）就跳过这一条，不误报。
    #[test]
    fn wired_knobs_have_a_reader() {
        // 把「读取点所在的 Rust 文件」列全。新增了读取点却没进这张表，就会
        // 误报成死键 —— 那时把文件名加进来即可，比放宽判据安全得多。
        const SOURCES: &[(&str, &str)] = &[
            ("mod.rs", include_str!("mod.rs")),
            ("model.rs", include_str!("model.rs")),
            ("search.rs", include_str!("search.rs")),
            ("index.rs", include_str!("index.rs")),
            ("store.rs", include_str!("store.rs")),
            ("layout.rs", include_str!("layout.rs")),
            ("import.rs", include_str!("import.rs")),
            ("import_registry.rs", include_str!("import_registry.rs")),
            ("sync.rs", include_str!("sync.rs")),
            ("schedule.rs", include_str!("schedule.rs")),
            ("enrich.rs", include_str!("enrich.rs")),
            ("llm.rs", include_str!("llm.rs")),
            ("inject.rs", include_str!("inject.rs")),
            ("retain.rs", include_str!("retain.rs")),
            ("agents.rs", include_str!("agents.rs")),
            ("rt.rs", include_str!("rt.rs")),
            ("redact.rs", include_str!("redact.rs")),
            ("api/memory.rs", include_str!("../../api/memory.rs")),
            ("tool_services/memory_mcp.rs", include_str!("../tool_services/memory_mcp.rs")),
        ];
        // 读取点在前端的旋钮：键 → 仓库内相对路径。后端的职责只是落盘并下发。
        const NON_RUST_READERS: &[(&str, &str)] = &[
            ("ui.tabs", "desktop-tauri/ui-islands/src/islands/memory-page.tsx"),
            ("ui.defaultTab", "desktop-tauri/ui-islands/src/islands/memory-page.tsx"),
            ("ui.pageSize", "desktop-tauri/ui-islands/src/islands/memory-page.tsx"),
        ];
        // 表里不能有拼错的键：拼错了就等于悄悄给一个死键开了后门。
        for (key, _) in NON_RUST_READERS {
            assert!(find(key).is_some(), "NON_RUST_READERS 里的 {key} 不是 schema 中的键");
        }
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .map(std::path::Path::to_path_buf);
        let mut missing: Vec<String> = Vec::new();
        for item in schema() {
            if !item.wired {
                continue;
            }
            // Rust 串用双引号，TSX 里习惯用单引号（`values['ui.pageSize']`），
            // 两种都算命中。
            let double = format!("\"{}\"", item.key);
            let single = format!("'{}'", item.key);
            if SOURCES
                .iter()
                .any(|(_, text)| text.contains(&double) || text.contains(&single))
            {
                continue;
            }
            let frontend = NON_RUST_READERS.iter().find(|(key, _)| *key == item.key);
            let Some((_, path)) = frontend else {
                missing.push(item.key.to_string());
                continue;
            };
            // 仓库根找得到就核对一遍（防「表里写着前端读了，其实人家早删了」）；
            // 找不到就放过 —— 只打服务端包时没有前端源码，那不是错误。
            if let Some(root) = &repo {
                let file = root.join(path);
                if let Ok(text) = std::fs::read_to_string(&file) {
                    if !text.contains(&double) && !text.contains(&single) {
                        missing.push(format!("{}（{} 里找不到）", item.key, path));
                    }
                }
            }
        }
        assert!(
            missing.is_empty(),
            "这些旋钮标了 wired=true，但源码里找不到 `\"key\"` 字面量读取点：{}\n\
             要么把读取点接上，要么用 `unwired(...)` 包起来并写清原因 —— \
             两者都不做，用户改了它不会有任何效果。",
            missing.join(", ")
        );
    }
}
