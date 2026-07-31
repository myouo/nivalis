import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { CustomSelect } from "../../src/components/CustomSelect";

afterEach(cleanup);

const options = [
  { value: "recent", label: "Recent" },
  { value: "name", label: "Name" },
  { value: "size", label: "Size" },
] as const;

describe("CustomSelect", () => {
  it("opens the page-drawn listbox and selects an option", () => {
    const onChange = vi.fn();
    render(
      <CustomSelect
        value="recent"
        options={options}
        onChange={onChange}
        ariaLabel="Sort by"
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "Sort by" }));
    expect(screen.getByRole("listbox", { name: "Sort by" })).toBeInTheDocument();

    fireEvent.click(screen.getByRole("option", { name: "Name" }));
    expect(onChange).toHaveBeenCalledWith("name");
    expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
  });

  it("supports keyboard navigation and selection", () => {
    const onChange = vi.fn();
    render(
      <CustomSelect
        value="recent"
        options={options}
        onChange={onChange}
        ariaLabel="Sort by"
      />,
    );

    const trigger = screen.getByRole("button", { name: "Sort by" });
    fireEvent.keyDown(trigger, { key: "ArrowDown" });
    const recent = screen.getByRole("option", { name: "Recent" });
    expect(recent).toHaveFocus();

    fireEvent.keyDown(recent, { key: "ArrowDown" });
    const name = screen.getByRole("option", { name: "Name" });
    expect(name).toHaveFocus();

    fireEvent.keyDown(name, { key: "Enter" });
    expect(onChange).toHaveBeenCalledWith("name");
    expect(trigger).toHaveFocus();
  });

  it("closes with Escape without changing the value", () => {
    const onChange = vi.fn();
    render(
      <CustomSelect
        value="recent"
        options={options}
        onChange={onChange}
        ariaLabel="Sort by"
      />,
    );

    const trigger = screen.getByRole("button", { name: "Sort by" });
    fireEvent.click(trigger);
    fireEvent.keyDown(screen.getByRole("option", { name: "Recent" }), {
      key: "Escape",
    });

    expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
    expect(onChange).not.toHaveBeenCalled();
    expect(trigger).toHaveFocus();
  });
});
