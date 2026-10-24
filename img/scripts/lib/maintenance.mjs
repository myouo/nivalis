import { spawn } from "node:child_process";
import { mkdir } from "node:fs/promises";
import { existsSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { experimental_readRawConfig } from "wrangler";
import { telegramRequest } from "../../shared/telegram.ts";

export const projectRoot = fileURLToPath(new URL("../../", import.meta.url));
export const maintenanceRoot = join(projectRoot, ".wrangler/maintenance");
const wrangler = join(projectRoot, "node_modules/wrangler/bin/wrangler.js");

export function loadSecrets() {
  const file = join(projectRoot, ".dev.vars");
  if (existsSync(file)) process.loadEnvFile(file);
  return {
    botToken: process.env.TELEGRAM_BOT_TOKEN?.trim() ?? "",
    chatId: process.env.TELEGRAM_CHAT_ID?.trim() ?? "",
  };
}

export async function inspectChannel(config) {
  const bot = await telegramRequest(config, "getMe");
  const chat = await telegramRequest(config, "getChat", { chat_id: config.chatId });
  if (chat.type !== "channel") throw new Error("TELEGRAM_CHAT_ID 必须指向存储频道。");
  if (String(chat.id) !== config.chatId) throw new Error("频道 ID 与配置不一致。");
  const member = await telegramRequest(config, "getChatMember", {
    chat_id: config.chatId, user_id: bot.id,
  });
  if (member.status !== "creator" &&
      (member.status !== "administrator" || !member.can_post_messages)) {
    throw new Error("请把 Bot 添加为频道管理员，并授予发布消息权限。");
  }
  return { bot, chat };
}

export async function readProjectConfig() {
  return experimental_readRawConfig({ config: join(projectRoot, "wrangler.jsonc") }).rawConfig;
}

export function redactSecrets(value) {
  let text = String(value);
  for (const name of ["TELEGRAM_BOT_TOKEN", "CF_API_TOKEN", "CLOUDFLARE_API_TOKEN", "SHARE_SIGNING_KEY"]) {
    const secret = process.env[name];
    if (secret) text = text.replaceAll(secret, "[REDACTED]");
  }
  return text;
}

export function runWrangler(args) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [wrangler, ...args], {
      cwd: projectRoot,
      env: { ...process.env, WRANGLER_SEND_METRICS: "false" },
      stdio: ["ignore", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => { stdout += chunk; });
    child.stderr.on("data", (chunk) => { stderr = (stderr + chunk).slice(-8000); });
    child.on("error", () => reject(new Error("无法启动 Wrangler。请先 npm install。")));
    child.on("close", (code) => {
      if (code === 0) resolve(stdout);
      else reject(new Error(redactSecrets(stderr || stdout || `Wrangler exited ${code}`)));
    });
  });
}

export function sqlValue(value) {
  if (value === null) return "NULL";
  if (typeof value === "number" && Number.isFinite(value)) return String(value);
  return `'${String(value).replaceAll("'", "''")}'`;
}

export async function queryDatabase(database, sql) {
  await mkdir(maintenanceRoot, { recursive: true, mode: 0o700 });
  // --file uses the bulk-import path, which does not return SELECT/RETURNING rows.
  // These statements contain storage identifiers, never credentials. spawn passes
  // them as one argument without invoking a shell.
  const output = await runWrangler([
    "d1", "execute", database, "--remote", "--json", "--command", sql,
  ]);
  const results = JSON.parse(output);
  if (!Array.isArray(results) || results.some((result) => !result.success)) {
    throw new Error("D1 query failed");
  }
  return results.flatMap((result) => result.results ?? []);
}
