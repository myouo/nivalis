import type { ImageVariant, ShareFormat } from "./types";

const CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const encoder = new TextEncoder();

function encodeTime(timestamp: number): string {
  let value = timestamp;
  let output = "";
  for (let index = 0; index < 10; index += 1) {
    output = CROCKFORD[value % 32] + output;
    value = Math.floor(value / 32);
  }
  return output;
}

function encodeRandom(): string {
  const bytes = new Uint8Array(10);
  crypto.getRandomValues(bytes);
  let buffer = 0;
  let bits = 0;
  let output = "";

  for (const byte of bytes) {
    buffer = (buffer << 8) | byte;
    bits += 8;
    while (bits >= 5) {
      bits -= 5;
      output += CROCKFORD[(buffer >>> bits) & 31];
    }
  }

  return output.slice(0, 16).padEnd(16, "0");
}

export function createUlid(timestamp = Date.now()): string {
  return `${encodeTime(timestamp)}${encodeRandom()}`;
}

function bytesToBase64Url(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) {
    binary += String.fromCharCode(byte);
  }
  return btoa(binary)
    .replaceAll("+", "-")
    .replaceAll("/", "_")
    .replace(/=+$/u, "");
}

function base64UrlToBytes(value: string): Uint8Array | null {
  try {
    const base64 = value.replaceAll("-", "+").replaceAll("_", "/");
    const padded = base64.padEnd(Math.ceil(base64.length / 4) * 4, "=");
    const binary = atob(padded);
    return Uint8Array.from(binary, (character) => character.charCodeAt(0));
  } catch {
    return null;
  }
}

async function hmac(
  secret: string,
  imageId: string,
  variant: ImageVariant,
  expiresAt: number,
  version: number,
  format: ShareFormat = "original",
  quality = 85,
): Promise<Uint8Array> {
  const key = await crypto.subtle.importKey(
    "raw",
    encoder.encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const basePayload = `${imageId}.${variant}.${expiresAt}.${version}`;
  const payload =
    format === "original"
      ? basePayload
      : `${basePayload}.${format}.${quality}`;
  return new Uint8Array(
    await crypto.subtle.sign("HMAC", key, encoder.encode(payload)),
  );
}

export async function createShareSignature(
  secret: string,
  imageId: string,
  variant: ImageVariant,
  expiresAt: number,
  version: number,
  format: ShareFormat = "original",
  quality = 85,
): Promise<string> {
  return bytesToBase64Url(
    await hmac(
      secret,
      imageId,
      variant,
      expiresAt,
      version,
      format,
      quality,
    ),
  );
}

export async function verifyShareSignature(
  secret: string,
  signature: string,
  imageId: string,
  variant: ImageVariant,
  expiresAt: number,
  version: number,
  now = Math.floor(Date.now() / 1000),
  format: ShareFormat = "original",
  quality = 85,
): Promise<boolean> {
  if (!Number.isSafeInteger(expiresAt) || expiresAt <= now) {
    return false;
  }

  const received = base64UrlToBytes(signature);
  if (!received) {
    return false;
  }

  const expected = await hmac(
    secret,
    imageId,
    variant,
    expiresAt,
    version,
    format,
    quality,
  );
  const [receivedHash, expectedHash] = await Promise.all([
    crypto.subtle.digest("SHA-256", received),
    crypto.subtle.digest("SHA-256", expected),
  ]);

  if ("timingSafeEqual" in crypto.subtle) {
    return crypto.subtle.timingSafeEqual(receivedHash, expectedHash);
  }

  const left = new Uint8Array(receivedHash);
  const right = new Uint8Array(expectedHash);
  let difference = 0;
  for (let index = 0; index < left.length; index += 1) {
    difference |= (left[index] ?? 0) ^ (right[index] ?? 0);
  }
  return difference === 0;
}

export function encodeCursor(createdAt: number, id: string): string {
  return bytesToBase64Url(encoder.encode(`${createdAt}:${id}`));
}

export function decodeCursor(
  cursor: string,
): { createdAt: number; id: string } | null {
  const bytes = base64UrlToBytes(cursor);
  if (!bytes) {
    return null;
  }
  const decoded = new TextDecoder().decode(bytes);
  const separator = decoded.indexOf(":");
  if (separator < 1) {
    return null;
  }
  const createdAt = Number(decoded.slice(0, separator));
  const id = decoded.slice(separator + 1);
  if (!Number.isSafeInteger(createdAt) || !id) {
    return null;
  }
  return { createdAt, id };
}
