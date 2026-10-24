import { createHash } from "node:crypto";
import { MAX_IMAGE_BYTES } from "./limits.ts";

export interface TelegramConfig {
  botToken: string;
  chatId: string;
}

export interface TelegramFileReference {
  file_id: string;
  file_unique_id: string;
  message_id: number;
  chat_id: string;
  bot_id: string;
  uploaded_at: number;
}

export class TelegramApiError extends Error {
  readonly retryAfter: number;
  readonly errorCode: number;

  constructor(method: string, errorCode: number, retryAfter = 0, detail = "") {
    // Never include upstream URLs, tokens, or arbitrary upstream error text.
    super(`Telegram ${method} failed (${errorCode})${detail ? `: ${detail}` : ""}`);
    this.name = "TelegramApiError";
    this.errorCode = errorCode;
    this.retryAfter = retryAfter;
  }
}

function networkErrorDetail(error: unknown, config: TelegramConfig): string {
  const message = error instanceof Error ? error.message : "Network request failed";
  return message.replaceAll(config.botToken, "[REDACTED]")
    .replaceAll(encodeURIComponent(config.botToken), "[REDACTED]").slice(0, 300);
}

export function telegramBotId(config: TelegramConfig): string {
  const match = config.botToken.match(/^(\d+):[A-Za-z0-9_-]+$/u);
  if (!match?.[1]) throw new Error("TELEGRAM_BOT_TOKEN is missing or invalid");
  return match[1];
}

export async function telegramRequest<T>(
  config: TelegramConfig,
  method: string,
  payload: FormData | Record<string, unknown> = {},
): Promise<T> {
  telegramBotId(config);
  let response: Response;
  try {
    response = await fetch(
      `https://api.telegram.org/bot${config.botToken}/${method}`,
      {
        method: "POST",
        ...(payload instanceof FormData
          ? { body: payload }
          : {
              body: JSON.stringify(payload),
              headers: { "Content-Type": "application/json" },
            }),
        signal: AbortSignal.timeout(60_000),
        redirect: "manual",
      },
    );
  } catch (error) {
    throw new TelegramApiError(method, 502, 0, networkErrorDetail(error, config));
  }
  const result = (await response.json().catch(() => null)) as {
    ok?: boolean;
    result?: T;
    error_code?: number;
    parameters?: { retry_after?: number };
  } | null;
  if (!response.ok || !result?.ok || result.result === undefined) {
    throw new TelegramApiError(
      method,
      result?.error_code ?? response.status,
      result?.parameters?.retry_after ?? 0,
    );
  }
  return result.result;
}

export async function uploadTelegramFile(
  config: TelegramConfig,
  bytes: ArrayBuffer,
  fileName: string,
  mimeType: string,
): Promise<TelegramFileReference> {
  if (!bytes.byteLength || bytes.byteLength > MAX_IMAGE_BYTES) {
    throw new RangeError("Telegram images must be between 1 byte and 20 MB");
  }
  if (!/^-\d+$/u.test(config.chatId)) {
    throw new Error("TELEGRAM_CHAT_ID must be a numeric channel ID");
  }
  const form = new FormData();
  form.set("chat_id", config.chatId);
  form.set("disable_notification", "true");
  form.set("disable_content_type_detection", "true");
  form.set("document", new Blob([bytes], { type: mimeType }), fileName);
  const message = await telegramRequest<{
    message_id: number;
    date: number;
    chat: { id: number };
    document?: { file_id: string; file_unique_id: string; file_size?: number };
  }>(config, "sendDocument", form);
  if (!message.document?.file_id || !Number.isSafeInteger(message.message_id)) {
    throw new Error("Telegram did not return a document reference");
  }
  return {
    file_id: message.document.file_id,
    file_unique_id: message.document.file_unique_id,
    message_id: message.message_id,
    chat_id: String(message.chat.id),
    bot_id: telegramBotId(config),
    uploaded_at: message.date * 1000,
  };
}

export async function downloadTelegramFile(
  config: TelegramConfig,
  fileId: string,
  range?: string,
): Promise<Response> {
  // Resolve a fresh temporary URL internally; never return it to the browser.
  const file = await telegramRequest<{ file_path?: string; file_size?: number }>(
    config,
    "getFile",
    { file_id: fileId },
  );
  if (!file.file_path || (file.file_size ?? 0) > MAX_IMAGE_BYTES) {
    throw new Error("Telegram file is unavailable or exceeds 20 MB");
  }
  const path = file.file_path.split("/");
  if (path.some((part) => !part || part === ".." || part === ".")) {
    throw new Error("Telegram returned an invalid file path");
  }
  let response: Response;
  try {
    response = await fetch(
      `https://api.telegram.org/file/bot${config.botToken}/${path.map(encodeURIComponent).join("/")}`,
      {
        headers: range ? { Range: range } : {},
        signal: AbortSignal.timeout(60_000),
        redirect: "manual",
      },
    );
  } catch (error) {
    throw new TelegramApiError("download", 502, 0, networkErrorDetail(error, config));
  }
  if (!response.ok || !response.body) {
    await response.body?.cancel().catch(() => undefined);
    throw new TelegramApiError("download", response.status);
  }
  return response;
}

export async function verifyTelegramFile(
  config: TelegramConfig,
  reference: TelegramFileReference,
  expectedSize: number,
  expectedHash: string,
): Promise<void> {
  const response = await downloadTelegramFile(config, reference.file_id);
  const hash = createHash("sha256");
  const reader = response.body!.getReader();
  let size = 0;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      size += value.byteLength;
      if (size > MAX_IMAGE_BYTES || size > expectedSize) {
        await reader.cancel().catch(() => undefined);
        throw new Error("Telegram returned an unexpected file size");
      }
      hash.update(value);
    }
  } finally {
    reader.releaseLock();
  }
  if (size !== expectedSize || hash.digest("hex") !== expectedHash) {
    throw new Error("Telegram round-trip verification failed; original storage retained");
  }
}
