import { afterEach, describe, expect, test } from "bun:test";
import { act, fireEvent, screen } from "@testing-library/react";
import { renderInApp } from "../../test/render.tsx";
import { ArrangedSections, arrange, moveTo, Section } from "./sections.tsx";

afterEach(() => localStorage.clear());

describe("arrange", () => {
  test("keeps the saved order of known ids, then adds new ids in their default order", () => {
    expect(arrange(undefined, ["a", "b", "c"])).toEqual(["a", "b", "c"]);
    expect(arrange(["c", "a"], ["a", "b", "c"])).toEqual(["c", "a", "b"]);
    expect(arrange(["gone", "b", "b"], ["a", "b"])).toEqual(["b", "a"]);
  });
});

describe("moveTo", () => {
  test("moves an id to an index, clamped to the list", () => {
    expect(moveTo(["a", "b", "c"], "a", 1)).toEqual(["b", "a", "c"]);
    expect(moveTo(["a", "b", "c"], "c", 0)).toEqual(["c", "a", "b"]);
    expect(moveTo(["a", "b", "c"], "b", 9)).toEqual(["a", "c", "b"]);
    expect(moveTo(["a", "b", "c"], "b", -1)).toEqual(["b", "a", "c"]);
    expect(moveTo(["a", "b"], "x", 0)).toEqual(["a", "b"]);
  });
});

const IDS = ["one", "two", "three"] as const;
const TITLES = { one: "First", two: "Second", three: "Third" };

const sections = () => (
  <ArrangedSections
    storageKey="test"
    ids={IDS}
    render={(id, state) => (
      <Section title={TITLES[id]} {...state}>
        <p>Body of {TITLES[id]}</p>
      </Section>
    )}
  />
);

const headings = () => screen.getAllByRole("heading").map((heading) => heading.textContent);

describe("arranged sections", () => {
  test("a heading collapses its section and the choice survives a reload", async () => {
    const first = await renderInApp(sections());
    const toggle = screen.getByRole("button", { name: "Second" });
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    await act(() => fireEvent.click(toggle));
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(screen.getByText("Body of Second").closest("[hidden]")).not.toBeNull();
    first.unmount();

    await renderInApp(sections());
    expect(screen.getByRole("button", { name: "Second" }).getAttribute("aria-expanded")).toBe(
      "false",
    );
    expect(screen.getByText("Body of First").closest("[hidden]")).toBeNull();
  });

  test("move buttons reorder sections, stop at the ends, and the order survives a reload", async () => {
    const first = await renderInApp(sections());
    expect(headings()).toEqual(["First", "Second", "Third"]);
    const up = screen.getByRole("button", { name: "Move First up" }) as HTMLButtonElement;
    expect(up.disabled).toBe(true);
    await act(() => fireEvent.click(screen.getByRole("button", { name: "Move First down" })));
    expect(headings()).toEqual(["Second", "First", "Third"]);
    await act(() => fireEvent.click(screen.getByRole("button", { name: "Move Third up" })));
    expect(headings()).toEqual(["Second", "Third", "First"]);
    first.unmount();

    await renderInApp(sections());
    expect(headings()).toEqual(["Second", "Third", "First"]);
    const down = screen.getByRole("button", { name: "Move First down" }) as HTMLButtonElement;
    expect(down.disabled).toBe(true);
  });

  test("unreadable saved state falls back to the defaults", async () => {
    localStorage.setItem("agent-board:test:order", "not json");
    localStorage.setItem("agent-board:test:collapsed", JSON.stringify({ one: true }));
    await renderInApp(sections());
    expect(headings()).toEqual(["First", "Second", "Third"]);
    expect(screen.getByRole("button", { name: "First" }).getAttribute("aria-expanded")).toBe(
      "true",
    );
  });
});
