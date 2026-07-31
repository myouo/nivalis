import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { DetailDrawer } from "../../src/components/DetailDrawer";
import { I18nProvider } from "../../src/i18n";
import type { ImageRecord } from "../../src/types";

const image: ImageRecord = {
  id: "image-1",
  originalName: "photo.jpg",
  slug: "photo.jpg",
  mimeType: "image/jpeg",
  byteSize: 1024,
  width: 640,
  height: 480,
  title: "Photo",
  altText: "",
  visibility: "private",
  status: "active",
  tags: [],
  createdAt: Date.UTC(2026, 6, 24),
  updatedAt: Date.UTC(2026, 6, 24),
  deletedAt: null,
  urls: {
    original: "/i/image-1/original/photo.jpg",
    thumbnail: "/api/media/image-1/w320/photo.jpg",
    preview: "/api/media/image-1/w1280/photo.jpg",
  },
};

function appearsBefore(first: Element, second: Element): boolean {
  return Boolean(
    first.compareDocumentPosition(second) & Node.DOCUMENT_POSITION_FOLLOWING,
  );
}

describe("DetailDrawer share layout", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });

  it("places one share-mode control before metadata save and keeps temporary-link controls together", () => {
    render(
      <I18nProvider>
        <DetailDrawer
          image={image}
          busy={false}
          onClose={vi.fn()}
          onSave={vi.fn().mockResolvedValue(undefined)}
          onShare={vi.fn().mockResolvedValue("/signed")}
          onRevoke={vi.fn().mockResolvedValue(undefined)}
          onTrash={vi.fn().mockResolvedValue(undefined)}
          onRestore={vi.fn().mockResolvedValue(undefined)}
          onDelete={vi.fn().mockResolvedValue(undefined)}
          onNotify={vi.fn()}
        />
      </I18nProvider>,
    );

    const shareMode = screen.getByText("Share mode");
    const save = screen.getByRole("button", { name: /save metadata/i });
    const temporary = screen.getByText("Generate temporary link");
    const expiry = screen.getByText("Expiry");
    const generate = screen.getByRole("button", {
      name: /generate and copy original link/i,
    });

    expect(screen.getAllByText("Share mode")).toHaveLength(1);
    expect(appearsBefore(shareMode, save)).toBe(true);
    expect(appearsBefore(save, temporary)).toBe(true);
    expect(appearsBefore(temporary, expiry)).toBe(true);
    expect(appearsBefore(expiry, generate)).toBe(true);
  });
});
