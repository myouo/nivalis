import { parseArgs } from "node:util";
import { loadSecrets, inspectChannel, redactSecrets } from "./lib/maintenance.mjs";
import {
  telegramRequest, uploadTelegramFile, verifyTelegramFile,
} from "../shared/telegram.ts";
import { sha256Hex } from "../shared/limits.ts";

const { values } = parseArgs({ options: { "test-upload": { type: "boolean" }, help: { type: "boolean" } } });

async function main() {
  if (values.help) {
    console.log("npm run telegram:check [-- --test-upload]\n读取 .dev.vars，发现频道 ID、检查管理员权限。--test-upload 会上传、校验并删除一张测试图片。");
    return;
  }
  const config = loadSecrets();
  if (!config.botToken) throw new Error("请先在 .dev.vars 中设置 TELEGRAM_BOT_TOKEN。");
  const bot = await telegramRequest(config, "getMe");
  console.log(`Bot: @${bot.username} (${bot.id})`);
  if (!config.chatId) {
    const webhook = await telegramRequest(config, "getWebhookInfo");
    if (webhook.url) throw new Error("这个 Bot 已配置 Webhook。请使用一个专用 Bot；工具不会修改已有 Webhook。");
    const updates = await telegramRequest(config, "getUpdates", { timeout: 0, limit: 100 });
    const chats = new Map();
    for (const update of updates) {
      const chat = update.channel_post?.chat ?? update.my_chat_member?.chat;
      if (chat?.type === "channel") chats.set(String(chat.id), chat.title);
    }
    for (const [id, title] of chats) console.log(`频道：${title}  TELEGRAM_CHAT_ID=${id}`);
    if (!chats.size) console.log("未找到频道：请将 Bot 加为频道管理员，在频道发一条消息，再运行此命令。");
    console.log("把目标频道 ID 写入 .dev.vars 的 TELEGRAM_CHAT_ID，然后重新运行检查。");
    process.exitCode = 1;
    return;
  }
  const { chat } = await inspectChannel(config);
  console.log(`频道：${chat.title} (${chat.id})；发布消息权限正常。`);
  if (values["test-upload"]) {
    const bytes = Uint8Array.from(Buffer.from(
      "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGNgAAIAAAUAAXpeqz8AAAAASUVORK5CYII=",
      "base64",
    )).buffer;
    const file = await uploadTelegramFile(config, bytes, "nivalis-connection-test.png", "image/png");
    try {
      await verifyTelegramFile(config, file, bytes.byteLength, await sha256Hex(bytes));
      console.log("测试图片上传、下载与 SHA-256 校验通过。");
    } finally {
      await telegramRequest(config, "deleteMessage", { chat_id: file.chat_id, message_id: file.message_id });
    }
    console.log("测试消息已删除。");
  }
}

main().catch((error) => {
  console.error(redactSecrets(error.message));
  process.exitCode = 1;
});
