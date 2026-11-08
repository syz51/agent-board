import { describe, expect, test } from "bun:test";
import { act, fireEvent, screen } from "@testing-library/react";
import { useState } from "react";
import { renderInApp } from "../../test/render.tsx";
import { TaskGroupSection } from "./board.tsx";

function Group() {
  const [collapsed, setCollapsed] = useState(true);
  return (
    <TaskGroupSection
      id="group-done"
      heading="Done"
      count={2}
      collapsed={collapsed}
      onToggle={() => setCollapsed(!collapsed)}
    >
      <p>Done tasks</p>
    </TaskGroupSection>
  );
}

describe("task groups", () => {
  test("a collapsible group's heading is a button that shows and hides its tasks", async () => {
    await renderInApp(<Group />);
    const toggle = screen.getByRole("button", { name: "Done 2" });
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(screen.getByText("Done tasks").closest("[hidden]")).not.toBeNull();
    await act(() => fireEvent.click(toggle));
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    expect(screen.getByText("Done tasks").closest("[hidden]")).toBeNull();
  });

  test("without onToggle the heading stays plain text", async () => {
    await renderInApp(
      <TaskGroupSection id="group-doing" heading="Doing" count={1}>
        <p>Doing tasks</p>
      </TaskGroupSection>,
    );
    expect(screen.queryByRole("button")).toBeNull();
    expect(screen.getByRole("heading", { name: "Doing 1" })).toBeTruthy();
  });
});
