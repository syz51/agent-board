import { describe, expect, test } from "bun:test";
import { fireEvent, render, screen } from "@testing-library/react";
import { ApiFailure } from "../api/client.ts";
import { ErrorNotice, ExpandableLine } from "./common.tsx";

describe("ErrorNotice", () => {
  test("with the connection up, says what failed and what to do, and retries", () => {
    let retries = 0;
    const error = new ApiFailure(500, "internal", "The board database is unavailable.", "wait");
    render(<ErrorNotice error={error} retry={() => retries++} />);
    expect(screen.getByText("The board database is unavailable.")).toBeTruthy();
    expect(screen.getByText("wait")).toBeTruthy();
    // Not the outage placeholder: that text shows only while the connection banner does.
    expect(screen.queryByText(/loads again once the connection is back/)).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(retries).toBe(1);
  });
});

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
