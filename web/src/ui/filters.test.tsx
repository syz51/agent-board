import { describe, expect, test } from "bun:test";
import { act, fireEvent, render, screen } from "@testing-library/react";
import { SearchField } from "./filters.tsx";

function setup(value = "") {
  const sent: string[] = [];
  const field = (current: string) => (
    <SearchField
      value={current}
      onChange={(next) => sent.push(next)}
      label="Search tasks"
      placeholder="Search"
    />
  );
  const view = render(field(value));
  const input = () => screen.getByRole("searchbox", { name: "Search tasks" }) as HTMLInputElement;
  const type = (text: string) => fireEvent.change(input(), { target: { value: text } });
  const url = (next: string) => act(() => view.rerender(field(next)));
  return { sent, input, type, url };
}

describe("search field", () => {
  test("keeps what was typed while the URL catches up with earlier keystrokes", () => {
    const field = setup();
    field.type("l");
    field.type("le");
    field.type("lea");
    expect(field.sent).toEqual(["l", "le", "lea"]);
    // The URL echoes the first keystroke after the third was typed.
    field.url("l");
    expect(field.input().value).toBe("lea");
    field.url("le");
    expect(field.input().value).toBe("lea");
    field.url("lea");
    expect(field.input().value).toBe("lea");
  });

  test("takes a value the URL got elsewhere, such as back navigation", () => {
    const field = setup("lease");
    expect(field.input().value).toBe("lease");
    field.type("leases");
    field.url("leases");
    field.url("");
    expect(field.input().value).toBe("");
    field.url("hooks");
    expect(field.input().value).toBe("hooks");
  });
});
