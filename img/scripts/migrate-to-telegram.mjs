import { mkdtemp, readFile, rm, stat } from "node:fs/promises";
import { join } from "node:path";
import { parseArgs } from "node:util";
import {
  inspectChannel, loadSecrets, maintenanceRoot, queryDatabase,
  readProjectConfig, redactSecrets, runWrangler, sqlValue as q,
} from "./lib/maintenance.mjs";
import {
  TelegramApiError, telegramBotId, telegramRequest,
  uploadTelegramFile, verifyTelegramFile,
} from "../shared/telegram.ts";
import { MAX_IMAGE_BYTES, sha256Hex } from "../shared/limits.ts";

const { values } = parseArgs({ options: {
  apply: { type: "boolean" },
  "delete-r2": { type: "boolean" },
  "prune-backups": { type: "boolean" },
  limit: { type: "string" },
  after: { type: "string", default: "" },
  help: { type: "boolean" },
} });
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function main() {
  if (values.help) {
    console.log(`npm run storage:migrate -- [options]
默认只统计远端数据，不修改数据库或发送 Telegram 消息。
  --apply                 迁移所有符合条件的活跃 R2 图片；保留 R2 副本
  --limit N               本次最多处理 N 张；重跑会跳过已迁移图片
  --after ID              从指定图片 ID 之后继续
  --delete-r2             校验成功并切换后，同时删除对应 R2 副本
  --prune-backups --apply  再次校验 TG 后，清除已迁移图片的 R2 副本
Token 和频道 ID 从 .dev.vars 或环境变量读取，不放在命令参数中。`);
    return;
  }
  if ((values["delete-r2"] || values["prune-backups"]) && !values.apply) {
    throw new Error("修改存储需要显式传入 --apply。");
  }
  const limit = values.limit === undefined ? Infinity : Number(values.limit);
  if (limit !== Infinity && (!Number.isSafeInteger(limit) || limit <= 0)) {
    throw new Error("--limit 必须是正整数。");
  }
  const project = await readProjectConfig();
  const database = project.d1_databases?.find((entry) => entry.binding === "DB")?.database_name;
  const bucket = project.r2_buckets?.find((entry) => entry.binding === "IMAGES_BUCKET")?.bucket_name;
  if (!database || !bucket) throw new Error("wrangler.jsonc 缺少 DB 或 IMAGES_BUCKET 配置。");
  const query = (sql) => queryDatabase(database, sql);
  const columns = await query("PRAGMA table_info(images)");
  const migratedSchema = columns.some((column) => column.name === "storage_backend");
  const backend = migratedSchema ? "storage_backend" : "'r2'";
  const summary = await query(`SELECT
    COUNT(*) AS total,
    COALESCE(SUM(CASE WHEN status = 'active' AND byte_size <= ${MAX_IMAGE_BYTES}
      AND ${backend} = 'r2' THEN 1 ELSE 0 END), 0) AS eligible,
    COALESCE(SUM(CASE WHEN byte_size > ${MAX_IMAGE_BYTES} THEN 1 ELSE 0 END), 0) AS retained_large,
    COALESCE(SUM(CASE WHEN ${backend} = 'telegram' THEN 1 ELSE 0 END), 0) AS on_telegram
    FROM images`);
  console.log(JSON.stringify({ mode: values.apply ? "apply" : "dry-run", ...summary[0] }));
  if (!values.apply) {
    if (!migratedSchema) console.log("正式迁移前需执行 npm run db:migrate:remote，应用 0003_telegram_storage.sql。");
    return;
  }
  if (!migratedSchema) throw new Error("请先应用 0003_telegram_storage.sql。");
  const config = loadSecrets();
  const { bot, chat } = await inspectChannel(config);
  console.log(`目标：@${bot.username} → ${chat.title} (${chat.id})`);
  const pruning = Boolean(values["prune-backups"]);
  let cursor = values.after;
  let attempted = 0;
  let completed = 0;
  let failed = 0;

  async function sendImage(bytes, image) {
    for (let attempt = 0; attempt < 3; attempt += 1) {
      const now = Date.now();
      const slots = await query(`INSERT INTO telegram_send_slots (chat_id, next_send_at)
        VALUES (${q(config.chatId)}, ${now + 1100})
        ON CONFLICT(chat_id) DO UPDATE SET next_send_at = MAX(next_send_at, ${now}) + 1100
        WHERE next_send_at <= ${now + 10_000} RETURNING next_send_at`);
      if (!slots[0]) throw new Error("频道发送队列繁忙，稍后重跑即可。");
      await sleep(Math.max(0, slots[0].next_send_at - 1100 - Date.now()));
      try {
        return await uploadTelegramFile(config, bytes, image.original_name, image.mime_type);
      } catch (error) {
        if (!(error instanceof TelegramApiError) || error.errorCode !== 429 ||
            attempt === 2 || error.retryAfter > 60) throw error;
        const delay = Math.max(1, error.retryAfter) * 1000;
        await query(`UPDATE telegram_send_slots SET next_send_at = MAX(next_send_at, ${Date.now() + delay})
          WHERE chat_id = ${q(config.chatId)}`);
        await sleep(delay);
      }
    }
    throw new Error("Telegram upload retries exhausted");
  }

  async function removeBackup(image, reference) {
    // A separate verification is intentional: pruning may run days after migration.
    await verifyTelegramFile(config, reference, image.byte_size, image.sha256);
    const current = await query(`SELECT id FROM images WHERE id = ${q(image.id)}
      AND storage_backend = 'telegram' AND status = 'active'`);
    if (!current[0]) throw new Error("图片状态已改变，保留 R2 副本。");
    await runWrangler(["r2", "object", "delete", `${bucket}/${image.object_key}`, "--remote"]);
    await query(`UPDATE telegram_objects SET r2_backup = 0 WHERE image_id = ${q(image.id)}
      AND file_id = ${q(reference.file_id)}`);
  }

  async function migrateImage(image) {
    let [reference] = await query(`SELECT * FROM telegram_objects WHERE image_id = ${q(image.id)}`);
    if (reference?.deleted_at) throw new Error("存在已删除的 TG 映射，请人工核对；保留 R2。");
    if (reference && (reference.bot_id !== telegramBotId(config) || reference.chat_id !== config.chatId)) {
      throw new Error("已有 TG 映射属于其他 Bot 或频道，保留 R2。");
    }
    if (pruning) {
      if (!reference?.verified_at || !reference.r2_backup) return;
      await removeBackup(image, reference);
      return;
    }
    if (!reference) {
      const directory = await mkdtemp(join(maintenanceRoot, "image-"));
      try {
        const file = join(directory, "original");
        await runWrangler(["r2", "object", "get", `${bucket}/${image.object_key}`, "--remote", "--file", file]);
        const info = await stat(file);
        if (info.size > MAX_IMAGE_BYTES || info.size !== image.byte_size) {
          throw new Error("R2 实际大小超限或与元数据不一致，保留 R2。");
        }
        const bytes = Uint8Array.from(await readFile(file)).buffer;
        if (await sha256Hex(bytes) !== image.sha256) throw new Error("R2 SHA-256 不一致，保留 R2。");
        reference = await sendImage(bytes, image);
        const inserted = await query(`INSERT INTO telegram_objects
          (image_id, file_id, file_unique_id, message_id, chat_id, bot_id, uploaded_at, r2_backup)
          SELECT ${q(image.id)}, ${q(reference.file_id)}, ${q(reference.file_unique_id)},
            ${q(reference.message_id)}, ${q(reference.chat_id)}, ${q(reference.bot_id)},
            ${q(reference.uploaded_at)}, 1
          WHERE EXISTS (SELECT 1 FROM images WHERE id = ${q(image.id)}
            AND storage_backend = 'r2' AND status = 'active' AND sha256 = ${q(image.sha256)})
          ON CONFLICT(image_id) DO NOTHING RETURNING image_id`);
        if (!inserted[0]) {
          await telegramRequest(config, "deleteMessage", { chat_id: reference.chat_id, message_id: reference.message_id });
          throw new Error("图片状态或映射已改变，保留 R2。");
        }
      } finally {
        await rm(directory, { recursive: true, force: true });
      }
    }
    // Checkpoints from interrupted runs are verified again rather than reuploaded.
    await verifyTelegramFile(config, reference, image.byte_size, image.sha256);
    await query(`UPDATE telegram_objects SET verified_at = ${Date.now()}
      WHERE image_id = ${q(image.id)} AND file_id = ${q(reference.file_id)} AND deleted_at IS NULL`);
    const switched = await query(`UPDATE images SET storage_backend = 'telegram'
      WHERE id = ${q(image.id)} AND storage_backend = 'r2' AND status = 'active'
      AND sha256 = ${q(image.sha256)} AND EXISTS (SELECT 1 FROM telegram_objects
        WHERE image_id = ${q(image.id)} AND file_id = ${q(reference.file_id)}
          AND verified_at IS NOT NULL AND deleted_at IS NULL)
      RETURNING id`);
    if (!switched[0]) throw new Error("图片状态已改变，未切换存储，保留 R2。");
    if (values["delete-r2"]) await removeBackup(image, reference);
  }

  while (attempted < limit) {
    const rows = await query(`SELECT i.* FROM images i
      ${pruning ? "JOIN telegram_objects t ON t.image_id = i.id AND t.r2_backup = 1 AND t.deleted_at IS NULL" : ""}
      WHERE i.status = 'active' AND i.storage_backend = '${pruning ? "telegram" : "r2"}'
        AND i.byte_size <= ${MAX_IMAGE_BYTES} AND i.id > ${q(cursor)}
      ORDER BY i.id LIMIT ${Math.min(25, limit - attempted)}`);
    if (!rows.length) break;
    for (const image of rows) {
      cursor = image.id;
      attempted += 1;
      try {
        await migrateImage(image);
        completed += 1;
        console.log(`${pruning ? "已清理副本" : "已迁移"} ${image.id}`);
      } catch (error) {
        failed += 1;
        console.error(`${image.id}: ${redactSecrets(error.message)}`);
      }
    }
  }
  console.log(JSON.stringify({ attempted, completed, failed, lastId: cursor }));
  if (failed) process.exitCode = 1;
}

main().catch((error) => {
  console.error(redactSecrets(error.message));
  process.exitCode = 1;
});
