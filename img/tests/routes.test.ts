import { describe, expect, it } from "vitest";
import { pathForView, viewFromPathname } from "../src/routes";

describe("application routes", () => {
  it.each([
    ["upload", "/upload"],
    ["gallery", "/gallery"],
    ["albums", "/albums"],
    ["trash", "/trash"],
    ["settings", "/settings"],
  ] as const)("maps %s to %s", (view, path) => {
    expect(pathForView(view)).toBe(path);
    expect(viewFromPathname(path)).toBe(view);
  });

  it("normalizes the root and trailing slashes", () => {
    expect(viewFromPathname("/")).toBe("upload");
    expect(viewFromPathname("/settings/")).toBe("settings");
  });

  it("keeps the old images route as a gallery alias", () => {
    expect(viewFromPathname("/images")).toBe("gallery");
  });

  it("rejects unknown paths", () => {
    expect(viewFromPathname("/unknown")).toBeNull();
  });
});
