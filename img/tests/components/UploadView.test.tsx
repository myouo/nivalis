import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { UploadView } from "../../src/components/UploadView";
import { I18nProvider } from "../../src/i18n";

function renderUploadView(
  props: Partial<React.ComponentProps<typeof UploadView>> = {},
) {
  return render(
    <I18nProvider>
      <UploadView
        items={[]}
        onFiles={() => undefined}
        onBrowse={() => undefined}
        onClear={() => undefined}
        onNotify={() => undefined}
        {...props}
      />
    </I18nProvider>,
  );
}

describe("UploadView", () => {
  it("opens the always-ready application file picker", () => {
    const onBrowse = vi.fn();
    renderUploadView({ onBrowse });
    fireEvent.click(
      screen.getByRole("button", { name: /click or drag files here/i }),
    );
    expect(onBrowse).toHaveBeenCalledOnce();
  });

  it("renders a real queue item", () => {
    const file = new File(["pixels"], "photo.png", { type: "image/png" });
    renderUploadView({
      items: [
        {
          id: "one",
          file,
          progress: 42,
          status: "uploading",
          message: "Uploading",
        },
      ],
    });
    expect(screen.getAllByText("photo.png")).toHaveLength(2);
  });

  it("does not render the removed copy-all action", () => {
    renderUploadView();
    expect(
      screen.queryByRole("button", { name: /copy all links/i }),
    ).not.toBeInTheDocument();
  });
});
