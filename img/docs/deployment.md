# Cloudflare 部署指南

Nivalis 采用匿名浏览器会话，不要求注册、邮箱或第三方登录。每个浏览器通过 HttpOnly Cookie 获得独立图库，只能管理自己上传的图片。

## 1. 准备域名与公开访问

假设使用：

- `img.nivalis.is`：公开上传与管理界面
- `simg.nivalis.is`：图片链接

两个域名都必须保持公开，不要为 `img.nivalis.is` 创建 Cloudflare Access Application。若此前已经创建匹配该域名的 Access Application，需要先移除；否则访客仍会被登录页拦截。

## 2. 配置 Worker

修改 [`wrangler.jsonc`](../wrangler.jsonc)：

```jsonc
{
  "vars": {
    "IMAGE_STORAGE": "telegram",
    "ADMIN_ORIGIN": "https://img.nivalis.is",
    "PUBLIC_IMAGE_ORIGIN": "https://simg.nivalis.is",
    "ZONE_ID": "域名所在 Zone 的 ID",
    "MAX_UPLOAD_BYTES": "20971520",
    "MAX_OWNER_STORAGE_BYTES": "1073741824",
    "MAX_OWNER_IMAGES": "500"
  },
  "routes": [
    {
      "pattern": "img.nivalis.is",
      "custom_domain": true
    },
    {
      "pattern": "simg.nivalis.is",
      "custom_domain": true
    }
  ]
}
```

默认额度为每个匿名浏览器 500 张、1 GiB，单张最大 20 MiB。已有超过 20 MiB 的图片继续保留在 R2。`ratelimits` 中还配置了会话创建、单浏览器上传和来源地址上传三组限流；可以按实际成本预算调整。

修改配置后执行：

```bash
npm run cf:typegen
npm run typecheck
```

不要把 API Token 或签名密钥写入 `wrangler.jsonc`。

## 3. 创建并授权缓存清除 Token

在 Cloudflare API Tokens 中创建自定义 Token：

- Zone Resources：只包含当前域名
- Permission：`Zone / Cache Purge / Purge`

这个 Token 仅用于在图片转私密、回收或删除时清除公开缓存。R2 和 D1 均通过 Worker bindings 访问，不使用 REST Token。

## 4. 首次部署与资源初始化

先登录：

```bash
npx wrangler login
```

当前生产环境已经创建：

- D1 数据库：`nivalis`
- R2 私有桶：`nivalis-images`

若需要在新账户中重新初始化，先用 Wrangler 创建资源并把返回的 D1 `database_id` 写入 `wrangler.jsonc`：

```bash
npx wrangler d1 create nivalis --location apac
npx wrangler r2 bucket create nivalis-images
npm run deploy
```

按 [Telegram 存储指南](./telegram.md) 创建私有频道与 Bot，然后交互式写入 Secret：

```bash
npx wrangler secret put SHARE_SIGNING_KEY
npx wrangler secret put CF_API_TOKEN
npx wrangler secret put TELEGRAM_BOT_TOKEN
npx wrangler secret put TELEGRAM_CHAT_ID
```

- `SHARE_SIGNING_KEY`：粘贴至少 32 个随机字符。
- `CF_API_TOKEN`：粘贴上一步创建的 Cache Purge Token。
- `TELEGRAM_BOT_TOKEN`：BotFather 创建的专用 Bot Token。
- `TELEGRAM_CHAT_ID`：私有频道的数字 ID，通常以 `-100` 开头。

命令会安全提示输入值；不要把值放在命令参数、脚本或 Git 中。

先备份并应用远端迁移，再部署代码。新代码依赖匿名所有权与存储路由字段：

```bash
npm run db:migrate:remote
npm run deploy
```

## 5. 上线核对

- 打开 `https://img.nivalis.is` 会直接进入 `/upload`，不出现登录页。
- 首次打开时，响应会设置 `nivalis_session` HttpOnly Cookie。
- 在两个不同浏览器中分别上传图片，双方图库不应看到或修改对方图片。
- 私密图片的 `/i/*` URL 返回 404。
- 生成 1 小时时效链接后，`/s/*` URL 可以匿名访问；撤销后立即返回 404。
- 将图片设为公开后，`simg.nivalis.is/i/*` 可以匿名访问。
- 再切回私密后，公开 URL 返回 404，且全部固定变体已清除。
- 清除 `img.nivalis.is` 的 Cookie 后，会得到一个新的空图库；旧图库管理权限无法恢复。
- R2 设置中 Public Development URL 保持关闭。
- Worker Observability 中能看到结构化请求错误，但不应出现 Cookie、签名或图片内容。

## 6. 公开服务的必要防护

- 在 Cloudflare Billing 中设置费用通知，并定期查看 R2 存储与操作量。
- 保留代码内的上传频率限制和每浏览器容量配额。
- 为 `img.nivalis.is/api/*` 创建 Cache Rule，选择 Bypass cache；Worker 同时会为这些响应发送 `private, no-store`。
- 建议为上传流程增加 Turnstile，并在 Worker 服务端验证令牌；仅放置前端组件不能阻止绕过。
- 可按国家、ASN、Bot 分数或异常流量在 WAF 中增加规则，但不要对 `simg.nivalis.is` 的正常图片读取使用过于严格的挑战。

## 运维命令

```bash
# 实时日志
npx wrangler tail

# 查看部署版本
npx wrangler versions list

# 检查启动时间
npm run check:startup

# 导出 D1 备份
npx wrangler d1 export nivalis --remote --output nivalis-backup.sql

# 手动触发本地定时任务测试
npx wrangler dev --test-scheduled
```

定时任务每天 03:15 UTC 执行，移除超过 24 小时未完成的上传和进入回收站超过 30 天的图片，清理对应 R2 文件。TG 消息超过 Bot API 删除窗口时会保留清理记录，详见 [Telegram 存储指南](./telegram.md)。

## 参考

- [Workers Static Assets](https://developers.cloudflare.com/workers/static-assets/)
- [R2 Workers API](https://developers.cloudflare.com/r2/api/workers/workers-api-reference/)
- [Cloudflare Images binding](https://developers.cloudflare.com/images/optimization/binding/)
- [Workers Rate Limiting binding](https://developers.cloudflare.com/workers/runtime-apis/bindings/rate-limit/)
- [Turnstile 服务端验证](https://developers.cloudflare.com/turnstile/get-started/server-side-validation/)
- [Wrangler 配置](https://developers.cloudflare.com/workers/wrangler/configuration/)
