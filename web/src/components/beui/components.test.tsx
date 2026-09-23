// @vitest-environment jsdom
import { fireEvent, render, screen } from "@testing-library/react";
import { beforeAll, describe, expect, it, vi } from "vitest";
import { Button, Drawer, Switch, Tabs } from "./index";

beforeAll(() => {
  // jsdom does not implement native modal dialogs; browser acceptance checks
  // actual top-layer focus containment and background inertness.
  HTMLDialogElement.prototype.showModal = function showModal() { this.setAttribute("open", ""); };
  HTMLDialogElement.prototype.close = function close() { this.removeAttribute("open"); };
});

describe("beUI console adaptations", () => {
  it("keeps buttons semantic and passes native button props", () => {
    const action = vi.fn();
    render(<Button variant="outline" type="submit" onClick={action}>Save</Button>);
    const button = screen.getByRole("button", { name: "Save" });
    expect(button.getAttribute("type")).toBe("submit");
    expect(button.className).toContain("beui-button--outline");
    fireEvent.click(button);
    expect(action).toHaveBeenCalledOnce();
  });

  it("selects tabs with Arrow, Home, and End and skips disabled items", () => {
    const onChange = vi.fn();
    const items = [
      { value: "use", label: "Usage" },
      { value: "disabled", label: "Unavailable", disabled: true },
      { value: "rules", label: "Rules" },
    ];
    const view = render(<Tabs items={items} value="use" onChange={onChange} label="Cache sections" />);
    const usage = screen.getByRole("tab", { name: "Usage" });
    usage.focus();
    fireEvent.keyDown(usage, { key: "ArrowRight" });
    expect(onChange).toHaveBeenLastCalledWith("rules");
    view.rerender(<Tabs items={items} value="rules" onChange={onChange} label="Cache sections" />);
    const rules = screen.getByRole("tab", { name: "Rules" });
    expect(rules.getAttribute("aria-selected")).toBe("true");
    fireEvent.keyDown(rules, { key: "Home" });
    expect(onChange).toHaveBeenLastCalledWith("use");
    fireEvent.keyDown(rules, { key: "End" });
    expect(onChange).toHaveBeenLastCalledWith("rules");
  });

  it("uses controlled switch state and respects disabled", () => {
    const onCheckedChange = vi.fn();
    const view = render(<Switch checked={false} onCheckedChange={onCheckedChange} label="Enable cache" />);
    const control = screen.getByRole("switch", { name: "Enable cache" });
    expect(control.getAttribute("aria-checked")).toBe("false");
    fireEvent.click(control);
    expect(onCheckedChange).toHaveBeenCalledWith(true);
    expect(control.getAttribute("aria-checked")).toBe("false");
    view.rerender(<Switch checked={true} onCheckedChange={onCheckedChange} label="Enable cache" disabled />);
    expect(control.getAttribute("aria-checked")).toBe("true");
    fireEvent.click(control);
    expect(onCheckedChange).toHaveBeenCalledTimes(1);
  });

  it("opens a modal drawer, closes immediately, and restores focus", () => {
    const trigger = document.createElement("button");
    trigger.textContent = "Details";
    document.body.append(trigger);
    trigger.focus();
    const onClose = vi.fn();
    const view = render(<Drawer open title="Query details" onClose={onClose} closeLabel="Close" >Content</Drawer>);
    const dialog = screen.getByRole("dialog", { name: "Query details" }) as HTMLDialogElement;
    expect(dialog.open).toBe(true);
    expect(dialog.getAttribute("aria-modal")).toBe("true");
    const close = screen.getByRole("button", { name: "Close" });
    expect(document.activeElement).toBe(close);
    fireEvent(dialog, new Event("cancel", { cancelable: true }));
    expect(onClose).toHaveBeenCalledTimes(1);
    fireEvent.pointerDown(dialog);
    expect(onClose).toHaveBeenCalledTimes(2);
    fireEvent.click(close);
    expect(onClose).toHaveBeenCalledTimes(3);
    view.rerender(<Drawer open={false} title="Query details" onClose={onClose}>Content</Drawer>);
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(document.activeElement).toBe(trigger);
    trigger.remove();
  });
});
