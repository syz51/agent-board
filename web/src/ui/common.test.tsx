import { describe, expect, test } from "bun:test";
import { fireEvent, render, screen } from "@testing-library/react";
import { ExpandableLine } from "./common.tsx";

// ErrorNotice is covered through the whole app, with the connection banner, in outage.test.tsx.

describe("ExpandableLine", () => {
  test("cuts to the lines asked for, and shows the whole text when pressed", () => {
    render(<ExpandableLine text="A long goal" lines={1} />);
    const line = screen.getByRole("button", { name: "A long goal" });
    expect(line.className).toContain("line-clamp-1");
    fireEvent.click(line);
    expect(line.getAttribute("aria-expanded")).toBe("true");
    expect(line.className).not.toContain("line-clamp");
  });
});
