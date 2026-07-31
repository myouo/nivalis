import type { ActiveView } from "./types";

export const VIEW_PATHS: Record<ActiveView, string> = {
  upload: "/upload",
  gallery: "/gallery",
  albums: "/albums",
  trash: "/trash",
  settings: "/settings",
};

const PATH_VIEWS = new Map(
  Object.entries(VIEW_PATHS).map(([view, path]) => [path, view as ActiveView]),
);

function normalizePathname(pathname: string): string {
  if (!pathname || pathname === "/") return "/";
  return `/${pathname.split("/").filter(Boolean).join("/")}`;
}

export function pathForView(view: ActiveView): string {
  return VIEW_PATHS[view];
}

export function viewFromPathname(pathname: string): ActiveView | null {
  const normalized = normalizePathname(pathname);
  if (normalized === "/") return "upload";
  if (normalized === "/images") return "gallery";
  return PATH_VIEWS.get(normalized) ?? null;
}
