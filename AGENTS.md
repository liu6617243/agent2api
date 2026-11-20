# AGENTS.md

> **涉及分支、发版、版本号的操作，一律按 Skill `release-flow` 执行：**
> **`.agents/skills/release-flow/SKILL.md`** —— 动手前先读它，细则一律以该文件为准，本文件只作入口指路。
>
> 覆盖范围：切分支 / 给 PR 选 base / 合并 PR / 回复 Issue、PR / 改版本号 / 写更新日志 /
> 打 tag / 挂 GitHub Release / 发 beta 预发布版 / 发版验收 / 排查发版失败。
>
> **涉及推代码后的 CI / 出包 / 交付自检，按 Skill `delivery-pipeline` 执行：**
> **`.agents/skills/delivery-pipeline/SKILL.md`** —— 交付三件套、闸门、状态查询、排错都在那里。
> 覆盖范围：推完代码 CI 跑没跑 / 安装包出没出 / artifact id 怎么拿 / workflow 触发面对不对。

## 两条红线（先问人，不得自行推进）

1. **发版**：改版本号、推 `main` / `beta`、打 tag、挂 Release 之前，先把版本号与「这次要发什么」讲清楚，等用户确认；**未经确认不得推任何分支或 tag** —— tag 一旦推出去就会触发构建、产出公开的安装包与镜像，收不回来。
2. **更新日志**：写好的日志（= 发布提交信息 = Release 正文）必须先给用户逐条过目；按反馈改完**再确认一次**才能提交。

其余细则（分支落脚点、署名与 AI 落款、六处版本号、正式版 / beta 完整流程、验收清单、已知坑与排查）见上述 Skill。

## 提交与交付（硬性操作纪律）

这一节写在这里而不是 `memory/` 里，是因为 **`AGENTS.md` 每轮都会进上下文，`memory/` 不会** —— 放在
`memory/` 的规则等于没写（已因此重复踩坑三次）。凡「必须每次都生效」的规矩一律写进本文件。

1. **「提交」= 本地 commit + `git push` 到 origin，一步做完。**
   不要在 commit 后反问「要推吗 / 推哪个分支」。仍然受上面两条红线约束（版本号 / tag / Release 前必须确认）。
2. **推送后不下载构建产物。** CI 绿了就是交付完成：**不要** `curl` exe / zip / 安装包到 `.workbuddy/dist/`
   或任何临时目录，也不要为了「校验」「验证一下」去下载。报「CI 绿 + run / artifact id」即可。
   只有用户**明确要产物文件**时才下。
3. **验证靠代码与数据库，不靠下载产物。** 要确认改动生效，读源码 / 读 `~/.agent2api/agent2api.db`
   里的实际数据，不要去拉构建出来的二进制。

### 交付一条命令：`node scripts/deliver.mjs`

推 `main-patched` **不要裸 `git push`**（`.githooks/pre-push` 会拦下来并报错）。交付入口只有一条：

```bash
node scripts/deliver.mjs          # 推代码 + 跑测试 + 等绿 + 出安装包
node scripts/deliver.mjs --no-push    # 已推过，只补跑链路
node scripts/deliver.mjs --no-build   # 只跑测试与产物同源，不出包
```

它做的整条链：

```
git push HEAD:main-patched
   ├─ 派发 rust-tests          ┐ 并行
   └─ 派发 frontend-artifacts  ┘
          ↓ 两个都绿才继续（任一失败 → 不派发出包，退出码 1）
     派发 build-windows → 打印安装包 artifact 文件名 / 大小 / id
```

- **为什么用派发而不是 push 触发**：三个工作流的 `push.branches` 写的是上游分支名
  `[main, beta]` / `[v3.3.0-beta.3]`，在本 fork（干活分支 `main-patched`）**从来不会因 push 自动跑**。
  治本是补触发面，但本机凭据是 classic PAT、scope 只有 `public_repo`，写 `.github/workflows/**`
  被 GitHub 拒（`without workflow scope`）。`public_repo` 够调 `workflow_dispatch`，所以走派发这条路 ——
  零新权限、当场可用。
- **触发面补丁已备好**：`.github/workflows/{rust-tests,frontend-artifacts,build-windows}.yml` 的改动在
  本地，等凭据补上 `workflow` scope 后跑 `node scripts/push-workflows.mjs --all` 写上去；
  那之后连派发这一步都不需要，本脚本仍可当闸门用。
- **闸门**：`build-windows` 的 `gate` job 跑 `scripts/ci-gate.mjs`，等前两道闸门对**同一个提交**都变绿
  才放行；任一失败则本条直接失败、**不产出 artifact**（该 job 随触发面补丁一起上线，派发路径的闸门由
  `deliver.mjs` 自己等）。
- **只看状态**：`node scripts/ci-watch.mjs`（`--wait` / `--artifact` / `--gate`）。

⚠️ **不要再出现「代码改完了但没出安装包」**。2026-11-20 之前，本 fork 的工作流 push 触发面写的是
`[main, beta]`（上游分支名），而实际干活的分支是 `main-patched` —— 于是三个流水线**从来不会因为 push 自动跑**，
历史上每个 run 都是手动 `workflow_dispatch`，每次都要用户提醒才想起出包。现在这条路已经堵死：
裸推会被钩子拦下，唯一入口会顺手把包出完。

## 反代提供商问题排查

收到反代提供商相关的问题反馈时，**优先使用网络搜索查找已有解决方案**，重点参考同类项目的 Issue、PR、修复记录和适配实现，了解其他项目是否遇到过相同问题、如何解决。**不要默认从零自行排查**：先核对已有方案与本项目的适用性，优先借鉴可靠的解决思路；未找到可靠方案、现有方案不适用或仍有未解决的问题时，再开展针对性的本地排查。

## 文档落点

安装 / 部署 / 升级 / 排错说明只在 `docs/install.md`（另有 5 份译文 `docs/install.{en,zh-Hant,ja,ko,pt-BR}.md`，六份一一对应）里维护；各语言 README 只留「快速开始」速查表 + 一个指向安装文档的链接，**不要再把安装细节写回 README**。产物名、端口、环境变量、安装脚本参数有改动时，六份一起改。

**README 语言落点：根 `README.md` 是英文**（GitHub 默认展示），简体中文在 `docs/README.zh-CN.md`，其余四份为 `docs/README.{zh-Hant,ja,ko,pt-BR}.md`。中文仍是文案源语言，改支持矩阵等内容时六份同步，别把中文写回根目录。
