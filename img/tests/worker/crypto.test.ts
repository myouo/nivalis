import { describe, expect, it } from "vitest";
import {
  createShareSignature,
  createUlid,
  decodeCursor,
  encodeCursor,
  verifyShareSignature,
} from "../../worker/crypto";

describe("share signatures", () => {
  it("accepts a valid signature and rejects tampering", async () => {
    const secret = "a-long-test-secret-that-is-not-production";
    const expiresAt = Math.floor(Date.now() / 1000) + 3600;
    const signature = await createShareSignature(
      secret,
      "image-id",
      "w640",
      expiresAt,
      3,
    );

    await expect(
      verifyShareSignature(
        secret,
        signature,
        "image-id",
        "w640",
        expiresAt,
        3,
      ),
    ).resolves.toBe(true);
    await expect(
      verifyShareSignature(
        secret,
        signature,
        "image-id",
        "w1280",
        expiresAt,
        3,
      ),
    ).resolves.toBe(false);
  });

  it("rejects expired signatures", async () => {
    const expiresAt = 100;
    const signature = await createShareSignature(
      "test-secret",
      "image-id",
      "original",
      expiresAt,
      1,
    );
    await expect(
      verifyShareSignature(
        "test-secret",
        signature,
        "image-id",
        "original",
        expiresAt,
        1,
        101,
      ),
    ).resolves.toBe(false);
  });

  it("binds WebP quality to a private share signature", async () => {
    const secret = "a-long-test-secret-that-is-not-production";
    const expiresAt = Math.floor(Date.now() / 1000) + 3600;
    const signature = await createShareSignature(
      secret,
      "image-id",
      "w1920",
      expiresAt,
      2,
      "webp",
      78,
    );

    await expect(
      verifyShareSignature(
        secret,
        signature,
        "image-id",
        "w1920",
        expiresAt,
        2,
        undefined,
        "webp",
        78,
      ),
    ).resolves.toBe(true);
    await expect(
      verifyShareSignature(
        secret,
        signature,
        "image-id",
        "w1920",
        expiresAt,
        2,
        undefined,
        "webp",
        79,
      ),
    ).resolves.toBe(false);
  });
});

describe("identifiers and cursors", () => {
  it("creates sortable 26-character ULIDs", () => {
    const earlier = createUlid(1_700_000_000_000);
    const later = createUlid(1_700_000_000_001);
    expect(earlier).toHaveLength(26);
    expect(later).toHaveLength(26);
    expect(earlier < later).toBe(true);
  });

  it("round-trips a cursor and rejects malformed values", () => {
    const cursor = encodeCursor(1_700_000_000_000, "01HABC");
    expect(decodeCursor(cursor)).toEqual({
      createdAt: 1_700_000_000_000,
      id: "01HABC",
    });
    expect(decodeCursor("not-valid%%%")).toBeNull();
  });
});
