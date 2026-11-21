#!/usr/bin/env node
// ─────────────────────────────────────────────────────────────────────
// rust-check.mjs —— 本机能不能真的编译 Rust，一条命令回答
//
// ── 它是什么地位 ──────────────────────────────────────────────────
// **可选加速，不是交付必经项。** 编译与构建的唯一权威是 GitHub Actions；
// 本机的 Rust 工具链已卸载（2026-11-21），本脚本默认什么都不做。
//
// 交付链路上它是 opt-in 的：
//
//   node scripts/deliver.mjs --local-rust-check
//
// 谁本机恰好有 cargo/rustc，谁就能在推之前先 check 一遍，
// 省下「推上去等三个平台各红 20 分钟」的一轮。
//
// ── 它补的是哪个洞 ────────────────────────────────────────────────
// `precheck-rust.mjs`（纯文本解析）抓得住「结构体字段没同步」这类文本级
// 问题，但抓不住**借用检查**：
//
//   error[E0507]: cannot move out of `other.id` which is behind a shared reference
//
// 2026-11-20 就是这样烧掉一整轮 CI —— 三个平台各自跑 20 分钟，全部红在同一条，
// 而本地从提交到推送一路畅通。
//
// ── 它做什么 ──────────────────────────────────────────────────────
//   1. 找工具链（rustup 代理桩常是 0 字节，必须校验真实可执行）
//   2. 校验 cargo/rustc 真能跑出版本号
//   3. 跑 `cargo check --workspace`（只做类型检查与借用检查，不链接）
//
// ── 为什么是 check 不是 build ─────────────────────────────────────
// `cargo check` 走完解析、宏展开、类型检查、借用检查，但**不生成机器码、不链接**。
// E0507 / E0382 / 生命周期错配全都在这一层暴露。代价是链接器（MSVC link.exe）
// 缺失时也能跑 —— 「代码能不能过编译器」这个最关键的判断不需要链接器。
//
// ── 用法 ───────────────────────────────────────────────────────────
//   node scripts/rust-check.mjs    # 有工具链就跑 check，没有就说明缺什么（退出码 2）
// ─────────────────────────────────────────────────────────────────────

import fs from 'node:fs'
import path from 'node:path'
import { execFileSync, spawnSync } from 'node:child_process'

// ── 定位与默认关闭 ──────────────────────────────────────────────
// 2026-11-21 决定：GitHub Actions 是唯一的构建权威，本机 Rust 工具链已卸载。
// 本脚本保留为**可选加速**：谁本机恰好有 cargo/rustc，谁就能先 check 一遍，
// 省下「推上去等三个平台各红 20 分钟」的一轮。
//
// 交付链路上它是 opt-in 的（node scripts/deliver.mjs --local-rust-check），
// 默认不跑 —— 交付不被本机环境绑死。
//
// ── 用法 ───────────────────────────────────────────────────────────
const ROOT = path.resolve(import.meta.dirname, '..')
// 只认环境变量和标准位置，不再硬编码某个本机专用的工具链目录。
const TOOLCHAIN_HOME = process.env.A2A_RUST_HOME || ''

// rustup 代理桩是 0 字节；真二进制一定有内容。用体积判，不靠名字。
const MIN_BIN_BYTES = 1024

function findRealBin(dir) {
  if (!fs.existsSync(dir)) return null
  for (const name of ['cargo.exe', 'cargo']) {
    const p = path.join(dir, name)
    if (!fs.existsSync(p)) continue
    const size = fs.statSync(p).size
    if (size < MIN_BIN_BYTES) continue
    return p
  }
  return null
}

/** 在几个候选位置找真 cargo。返回 {cargo, rustc, how} 或 null。 */
function locate() {
  const candidates = []

  // 1. 独立工具链目录（仅当显式指定 A2A_RUST_HOME）
  if (TOOLCHAIN_HOME) {
    const tcRoot = path.join(TOOLCHAIN_HOME, 'rustup', 'toolchains')
    if (fs.existsSync(tcRoot)) {
      for (const tc of fs.readdirSync(tcRoot)) {
        const bin = path.join(tcRoot, tc, 'bin')
        const cargo = findRealBin(bin)
        if (cargo) candidates.push({ cargo, rustc: path.join(bin, 'rustc.exe'), how: `toolchain ${tc}` })
      }
    }
  }

  // 2. 标准的 rustup 布局（代理桩会在体积判据里被滤掉）
  const home = process.env.USERPROFILE || process.env.HOME || ''
  if (home) {
    const cargo = findRealBin(path.join(home, '.cargo', 'bin'))
    if (cargo) candidates.push({ cargo, rustc: path.join(home, '.cargo', 'bin', 'rustc.exe'), how: '~/.cargo/bin' })
  }

  // 3. PATH 上的（可能是真二进制）
  for (const dir of (process.env.PATH || '').split(path.delimiter)) {
    const cargo = findRealBin(dir)
    if (cargo && !candidates.some((c) => c.cargo === cargo)) {
      candidates.push({ cargo, rustc: path.join(dir, 'rustc.exe'), how: 'PATH' })
    }
  }

  // 校验真能跑出版本号 —— 体积够不代表能用（可能是别的 exe 改名、或 DLL 缺失）
  for (const c of candidates) {
    try {
      const out = execFileSync(c.cargo, ['--version'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] })
      if (!/^cargo \d/.test(out.trim())) continue
      const rc = execFileSync(c.rustc, ['--version'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] })
      return { ...c, cargoVersion: out.trim(), rustcVersion: rc.trim() }
    } catch {
      /* 换下一个 */
    }
  }
  return null
}

const found = locate()

if (!found) {
  console.log('本机没有可用的 Rust 工具链。')
  console.log('')
  console.log('这是**预期状态**，不是故障：编译与构建的唯一权威是 GitHub Actions。')
  console.log('')
  console.log('本机能做的：')
  console.log('  node scripts/precheck-rust.mjs    # 静态检查（结构体字段、mod 路径），不需工具链')
  console.log('本机不做的：')
  console.log('  借用检查（E0507 / E0382 / 生命周期）—— 由 CI 的 rust-tests 兜底。')
  console.log('')
  console.log('交付链路默认不跑本机编译闸（node scripts/deliver.mjs 直接推、派发、等绿、出包）。')
  console.log('若本机恰好有工具链，可以显式要求先 check 一遍：')
  console.log('  node scripts/deliver.mjs --local-rust-check')
  console.log('')
  console.log('想在本机装的话（无需管理员、不污染 PATH）：')
  console.log('  1. 下载 https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe')
  console.log('  2. RUSTUP_HOME=<目录>\\rustup CARGO_HOME=<目录>\\cargo rustup-init.exe -y --no-modify-path --default-toolchain none')
  console.log('  3. <目录>\\cargo\\bin\\rustup.exe toolchain install stable --profile minimal')
  console.log('  4. 设 A2A_RUST_HOME=<目录> 再跑一次本脚本')
  process.exit(2)
}

console.log(`工具链：${found.how}`)
console.log(`  ${found.cargoVersion}`)
console.log(`  ${found.rustcVersion}`)

// CARGO_HOME 落在项目内 —— 沙箱环境只允许写项目目录，
// 落在用户目录会在解包 crate 时被拒（os error 5）。
const cargoHome = path.join(ROOT, 'target', 'cargo-home')
const targetDir = path.join(ROOT, 'target')
fs.mkdirSync(cargoHome, { recursive: true })

const env = {
  ...process.env,
  CARGO_HOME: cargoHome,
  CARGO_TARGET_DIR: targetDir,
  CARGO_NET_GIT_FETCH_WITH_CLI: 'true',
}

const crateDir = path.join(ROOT, 'desktop-tauri', 'src-tauri')
const started = Date.now()
console.log('')
console.log('cargo check --workspace（只做类型检查与借用检查，不链接）…')

const run = spawnSync(found.cargo, ['check', '--workspace'], {
  cwd: crateDir,
  env,
  stdio: 'inherit',
})

const secs = ((Date.now() - started) / 1000).toFixed(0)
console.log('')
if (run.status === 0) {
  console.log(`cargo check 通过（${secs}s）。借用检查这一关本机已经过了。`)
  process.exit(0)
}

console.log(`cargo check 失败（${secs}s）。上面的报错就是 CI 会红的原因，别推。`)
process.exit(1)
