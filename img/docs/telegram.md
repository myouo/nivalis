# Telegram 存储与历史图片迁移

新图片最多 20 MiB，通过 `sendDocument` 原样存入私有频道。D1 保存文件引用和图库权限，Worker 代理所有图片访问。已有超过 20 MiB 的图片继续从 R2 读取，图片 ID、原有链接、分享版本与有效期均不因迁移而改变。

## 创建频道和 Bot

1. 在官方 [@BotFather](https://t.me/BotFather) 发送 `/newbot`，创建专用 Bot，保存 Token。
2. 创建一个私有频道，将 Bot 添加为管理员并授予“发布消息”权限。频道无需公开，无需设置 Webhook。
3. 在 `.dev.vars` 中填写 `TELEGRAM_BOT_TOKEN`。Token 不要放在仓库、命令参数、浏览器 URL 或聊天记录中。
4. 在频道发一条消息，然后运行 `npm run telegram:check`。工具从 Bot 更新中列出频道数字 ID；将目标 ID 填入 `.dev.vars` 的 `TELEGRAM_CHAT_ID`。
5. 运行 `npm run telegram:check -- --test-upload`，检查频道权限，并上传、下载、校验、删除一张小测试图片。

工具使用 Node.js 24.5+ 和 `--use-env-proxy`，遵循 HTTP_PROXY/HTTPS_PROXY。已配置 Webhook 的其他用途 Bot 不宜直接复用；工具不会移除已有 Webhook。

```dotenv
TELEGRAM_BOT_TOKEN=填写你自己的Token
TELEGRAM_CHAT_ID=-100xxxxxxxxxx
```

生产环境通过 `npx wrangler secret put TELEGRAM_BOT_TOKEN` 和 `npx wrangler secret put TELEGRAM_CHAT_ID` 交互配置。保留已有签名密钥，不要为迁移重新生成，否则历史私密链接会失效。

## 部署和迁移

1. 用 Wrangler 导出 D1 备份，保存到 `.wrangler/backups/` 等已忽略目录。
2. 执行 `npm run db:migrate:remote`，应用 `0003_telegram_storage.sql`。迁移只新增字段、映射表与发送时间槽，旧记录默认仍走 R2。
3. 设置生产 TG Secrets，执行 `npm run deploy`。`IMAGE_STORAGE=telegram` 控制新上传；改为 `r2` 可回退新上传的存储目的地，上限仍为 20 MiB。
4. 执行 `npm run storage:migrate`，只统计远端数据。
5. 先迁移少量图片：`npm run storage:migrate -- --apply --limit 3`。工具从 R2 读取原图，检查实际大小和 SHA-256，上传 TG 并保存检查点，再回读 TG 核对完整内容，成功后才切换 D1 存储位置。
6. 核对旧链接和私密访问后，执行 `npm run storage:migrate -- --apply` 继续迁移全部符合条件的图片。
7. 默认保留 R2 副本，TG 暂不可用时自动回退读取。确认稳定后，执行 `npm run storage:migrate -- --prune-backups --apply`；每份副本删除前会再次回读 TG 校验。也可在迁移时显式传 `--delete-r2`。

每次最多查询 25 张图片，可通过 `--limit N` 限制本次处理量。中断后直接重跑：已迁移图片跳过，已上传但尚未切换的检查点会复用并重新校验。`--after ID` 可从指定 ID 后继续。不要同时运行多个迁移进程。

仅迁移 `active` 状态、D1 声明大小与 R2 实际大小一致且 SHA-256 正确、大小不超过 20 MiB 的图片。回收站和未完成上传不迁移。任何读取、上传、校验失败都会保留 R2；失败条目可在处理原因后重跑。

运行脚本前确保 Wrangler 的当前凭据对目标 D1、R2、Worker 有权限。若全局 `CLOUDFLARE_API_TOKEN` 覆盖了已有 OAuth 登录且缺少权限，可仅对该命令移除它，例如 `env -u CLOUDFLARE_API_TOKEN npm run storage:migrate`。

## 缓存、权限与回退

Worker 先检查 D1 图片状态、所属浏览器或私密签名，再查内部原图缓存。缓存键不包含 Bot Token，TG 临时下载地址也不会返回浏览器。原图 ETag 基于 SHA-256，保持迁移前后一致；缩略图继续由 Images 绑定处理。

保留 R2 副本时，可将符合条件的图片切回 R2；先确认 `telegram_objects.r2_backup=1` 且 R2 文件存在。已清除 R2 副本的图片需要先从 TG 校验并复制回 R2，再切换，不能直接改数据库路由。不要更换为另一个 Bot 后丢弃旧 Token：文件引用属于原 Bot；同一个 Bot 轮换 Token 不改变 Bot ID。

## 删除行为

移入回收站或永久移除后，网站会阻止原图、变体及签名链接访问。永久删除清理 R2 文件；对 TG 消息尝试 Bot API 删除。超过 48 小时的消息无法按此 API 删除，原消息可能仍存在于私有频道，需频道管理员手动清理。

`telegram_objects` 不级联删除，保留 `image_id`、`chat_id`、`message_id`、`deleted_at` 与 `cleanup_error` 供人工审计。可查询需要清理的记录：

```sql
SELECT image_id, chat_id, message_id, cleanup_error
FROM telegram_objects
WHERE deleted_at IS NOT NULL AND cleanup_error IS NOT NULL;
```

## 官方参考

- [Telegram getFile 与 20 MB 下载限制](https://core.telegram.org/bots/api#getfile)
- [Telegram deleteMessage 与 48 小时窗口](https://core.telegram.org/bots/api#deletemessage)
- [Telegram 发送频率限制](https://core.telegram.org/bots/faq#my-bot-is-hitting-limits-how-do-i-avoid-this)
- [Cloudflare Workers Secrets](https://developers.cloudflare.com/workers/configuration/secrets/)
