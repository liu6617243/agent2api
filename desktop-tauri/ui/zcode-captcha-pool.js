/* Agent2API · ZCode 活动套餐通道的**验证码令牌池守卫**

   ── 它解决的是什么（一句话）───────────────────────────────────
   ZCode 的「活动套餐」额度只从 `POST /api/v1/zcode-plan/anthropic/v1/messages`
   花得出去，而那个端点**每条请求都要一个当次铸的阿里云验证码令牌**：少它一律
   `400 {"code":3007,"msg":"captcha verify failed"}`（实测不是偶发挑战，是常规门禁）。
   Rust 侧铸不出来（要跑阿里云 SDK），所以由这个跑在 WebView 里的守卫铸：
   静默无痕验证 → 推给网关 → 转发层按请求取一个。

   ── 节奏（不铸多、不空窗）─────────────────────────────────────
   每轮问网关 `GET /api/zcode/captcha`（间隔与单轮产量都是网关下发的设置，
   见「节奏参数」段）：
     · `needsTokens: true`（有账号走活动套餐，且库存低于目标）→ 补到目标；
     · `needsTokens: false` → 什么都不做（**没有账号用这条路时一个都不铸** ——
       阿里云对铸造频率有风控，白铸纯属烧配额，见参考实现的熔断那一段）；
     · 上游刚回过 3007（`rejected` 计数变大）→ 立刻补一轮，不等下一个周期。
   令牌寿命两分钟（网关那侧定的），每次只铸到目标（默认 3 个）：
   「够下一条请求立刻有得用」即可。三项节奏可在设置页「网关 → 验证码
   令牌池」调（config 键 zcodeCaptchaPool*），随每轮 stats 下发热生效。

   ── 为什么不做成用户可见的按钮 ───────────────────────────────
   转发是后台发生的（用户在别的工具里发请求），没有人会在每条请求前点一次验证。
   这与「领套餐」的滑块不是一回事：那个是**用户动作**，必须让用户看见、确认；
   这个是**机器对机器的门禁令牌**，无痕模式本来就是为这种场景设计的。

   ── 失败时怎么办（如实，不装死）───────────────────────────────
   铸造失败（SDK 加载不出来、网络不通、阿里云限流）只写控制台并退让到下一个
   周期：令牌池空了，转发层会如实回一句「活动套餐通道需要人机验证令牌…」，
   用户知道去哪儿看。**不要**在这里弹 toast —— 它是后台循环，弹一次就会有
   第二次、第三次。

   依赖：`ui/aliyun-captcha.js` 的 `window.wbAliyunCaptcha.mintTraceless`
   （脚本顺序：本文件必须排在它之后）与桥接层 `zcodeCaptchaStats` /
   `pushZcodeCaptchaTokens`（deploy 的两种形态各有一份实现：桌面端
   desktop-tauri/src-tauri/src/bridge.rs，headless 面板
   server/src/web_shim.rs —— 两份都给才谈得上「同一份界面代码两种部署通用」，
   早先在 headless 缺这两条，于是铸造器每轮都退让、池子永远是空的，
   见 Issue #163）。 */

(() => {
  /** 轮询间隔的**回退默认**（毫秒）：网关没给 `pollMs` 时才用。正式值是
     设置页「网关 → 验证码令牌池」的设置项，随每轮 stats 下发（见 pollMsOf） */
  const POLL_MS = 2500;
  /** 单轮铸造上限的**回退默认**：网关没给 `maxPerRound` 时才用。1 = 单实例
     单次铸造，避免并发冲突与 DOM 重建竞态（见 maxPerRoundOf） */
  const MAX_PER_ROUND = 1;
  /** 节奏参数的合法范围（与后端 config 的范围常量同源：zcodeCaptchaPool*）。
     网关写侧已按这个范围校验过，这里的夹取只是防御响应被改坏 */
  const POLL_MIN_MS = 1000;
  const POLL_MAX_MS = 30000;
  const PER_ROUND_MIN = 1;
  const PER_ROUND_MAX = 5;

  /** 本轮的轮询间隔：网关设置说了算，缺失 / 非法回落默认并夹进合法范围 */
  function pollMsOf(stats) {
    const value = Number(stats.pollMs);
    if (!Number.isFinite(value) || value <= 0) return POLL_MS;
    return Math.min(POLL_MAX_MS, Math.max(POLL_MIN_MS, Math.round(value)));
  }

  /** 本轮的单轮铸造上限：同上 */
  function maxPerRoundOf(stats) {
    const value = Number(stats.maxPerRound);
    if (!Number.isFinite(value) || value <= 0) return MAX_PER_ROUND;
    return Math.min(PER_ROUND_MAX, Math.max(PER_ROUND_MIN, Math.round(value)));
  }
  /** 失败后的退让（毫秒）：失败时轻度退让，并在连续失败时重置铸造实例 */
  const BACKOFF_MIN_MS = 3000;
  const BACKOFF_MAX_MS = 15000;

  let timer = 0;
  let running = false;
  let stopped = false;
  /** 上一轮看到的 3007 计数（变大 = 上游刚拒了令牌，立刻补货） */
  let lastRejected = 0;
  /** 连续失败次数（退让用） */
  let failures = 0;
  let mintStatus = { state: 'idle', error: '' };

  function setStatus(state, error = '') {
    mintStatus = { state, error: String(error || '') };
  }

  const api = () => window.workbuddyDesktop || null;

  function log(message, ...args) {
    console.log(`[ZCodeCaptcha] ${message}`, ...args);
    if (window.wbApp?.debug) window.wbApp.debug(`[ZCodeCaptcha] ${message}`);
  }

  function warn(message, ...args) {
    console.warn(`[ZCodeCaptcha] ${message}`, ...args);
    if (window.wbApp?.debug) window.wbApp.debug(`[ZCodeCaptcha] [WARN] ${message}`);
  }

  async function fetchStats() {
    const bridge = api();
    if (!bridge?.zcodeCaptchaStats) throw new Error('验证码令牌池接口不可用');
    const stats = await bridge.zcodeCaptchaStats();
    if (!stats || typeof stats !== 'object') throw new Error('验证码令牌池未返回状态');
    if (stats.error) throw new Error(String(stats.error));
    return stats;
  }

  async function pushTokens(tokens) {
    const bridge = api();
    if (!bridge?.pushZcodeCaptchaTokens) throw new Error('验证码令牌入池接口不可用');
    const outcome = await bridge.pushZcodeCaptchaTokens(tokens);
    if (Number(outcome?.accepted) !== tokens.length) throw new Error('验证码令牌未全部入池');
  }

  async function mintOne(config) {
    const captcha = window.wbAliyunCaptcha;
    if (!captcha?.mintTraceless) throw new Error('验证码组件尚未就绪');
    if (captcha.isBusy?.()) return null;
    return captcha.mintTraceless(config);
  }

  /** 一轮：读概况 → 需要就补 → 排下一次 */
  async function round() {
    if (running || stopped) return;
    running = true;
    try {
      if (mintStatus.state !== 'error') setStatus('checking');
      const stats = await fetchStats();
      const rejectedNow = Number(stats.rejected) || 0;
      const challenged = rejectedNow > lastRejected;
      lastRejected = rejectedNow;
      const want = stats.needsTokens === true
        || (challenged && stats.required !== false && Number(stats.startPlanAccounts) > 0);
      if (!want) {
        failures = 0;
        setStatus(stats.required === false ? 'disabled' : 'idle');
        schedule(pollMsOf(stats));
        return;
      }
      // 风控配置只在真要铸的时候才去取（带 5 分钟缓存；上游那份配置很少变）
      const config = await fetchCaptchaConfig(stats);
      if (!config) {
        failures = 0;
        setStatus('disabled');
        schedule(pollMsOf(stats));
        return;
      }
      if (window.wbAliyunCaptcha?.isBusy?.()) {
        setStatus('waiting');
        schedule(pollMsOf(stats));
        return;
      }
      const target = Math.max(1, Number(stats.target) || 1);
      const ready = Number(stats.ready) || 0;
      const need = Math.min(maxPerRoundOf(stats), Math.max(0, target - ready));
      setStatus('minting');
      for (let index = 0; index < need; index += 1) {
        const param = await mintOne(config);
        if (!param) {
          setStatus('waiting');
          break;
        }
        await pushTokens([{ param, region: config.region || '' }]);
        await new Promise(resolve => window.setTimeout(resolve, 500));
      }
      failures = 0;
      if (mintStatus.state !== 'waiting') setStatus('idle');
      schedule(pollMsOf(stats));
    } catch (error) {
      failures += 1;
      setStatus('error', error?.message || error);
      if (failures >= 2) {
        try { window.wbAliyunCaptcha?.resetMint?.(); } catch {}
      }
      warn(`本轮异常：${error?.message || error}`);
      schedule(Math.min(BACKOFF_MIN_MS * failures, BACKOFF_MAX_MS));
    } finally {
      running = false;
    }
  }

  /** 风控配置的缓存（上游那份配置一天也不会变几次，每轮都问纯属浪费） */
  let configCache = { value: null, expiresAt: 0 };
  const CONFIG_TTL_MS = 5 * 60000;

  /**
   * 取阿里云风控配置（`{enabled, prefix, sceneId, region}`）。
   *
   * 与「领套餐」走的是**同一份**上游配置（`GET /api/v1/client/configs` 的
   * `configs.captcha`，见 `api::zcode_claim::captcha_config`）。账号 id 由网关
   * 给（`captchaAccountId`，它挑的是走活动套餐的那个）—— 界面不必自己读账号表。
   */
  async function fetchCaptchaConfig(stats) {
    if (configCache.value && configCache.expiresAt > Date.now()) return configCache.value;
    const bridge = api();
    const accountId = String(stats.captchaAccountId || '');
    if (!bridge?.zcodeClaimCaptchaConfig || !accountId) throw new Error('没有可用账号获取验证码配置');
    const config = await bridge.zcodeClaimCaptchaConfig(accountId);
    if (config?.enabled === false) {
      log('上游风控配置未启用（enabled:false）——不铸造');
      return null;
    }
    if (!config?.enabled || !config.sceneId || !config.prefix) {
      throw new Error('上游验证码配置不完整');
    }
    configCache = {
      value: { sceneId: config.sceneId, prefix: config.prefix, region: config.region },
      expiresAt: Date.now() + CONFIG_TTL_MS,
    };
    return configCache.value;
  }

  function schedule(delay) {
    window.clearTimeout(timer);
    if (stopped) return;
    timer = window.setTimeout(() => void round(), delay);
  }

  /** 起来（应用启动时调一次；重复调只是重置计时器） */
  function start() {
    stopped = false;
    schedule(1500);
  }

  function stop() {
    stopped = true;
    setStatus('idle');
    window.clearTimeout(timer);
  }

  window.wbZcodeCaptchaPool = { start, stop, status: () => ({ ...mintStatus }) };

  // 自启：界面加载完就开始守着（桌面端与 headless 面板都走这里 —— 两种部署的
  // 桥接方法都由各自的实现提供，见文件头）。桥接方法缺失时 fetchStats 抛错，
  // 循环按退让节奏重试，不会刷屏（错误只落控制台与 mintStatus）
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', start, { once: true });
  } else {
    start();
  }
})();
