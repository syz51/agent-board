import { afterEach, describe, expect, test } from "bun:test";
import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { DevState } from "../../dev/state.ts";
import { renderInApp } from "../../test/render.tsx";
import { AnswerForm, answerOf } from "./asks.tsx";

const state = new DevState(() => Date.parse("2026-11-06T12:00:00Z"));
const asks = state.asks(new URLSearchParams({ status: "open" }));
const withOptions = asks.find((ask) => ask.options.length > 1)!;
const original = globalThis.fetch;
afterEach(() => {
  globalThis.fetch = original;
  Reflect.deleteProperty(document, "execCommand");
});

describe("answerOf", () => {
  test("written text wins over a chosen option, and blank text does not count", () => {
    expect(answerOf({ choice: "Yes", text: "  No, wait  " })).toBe("No, wait");
    expect(answerOf({ choice: "Yes", text: "   " })).toBe("Yes");
    expect(answerOf({ choice: "", text: "" })).toBe("");
  });
});

describe("answer form", () => {
  test("sending nothing says what to do, marks the field and focuses it; a change clears it", async () => {
    await renderInApp(<AnswerForm ask={withOptions} />);
    await act(() => fireEvent.click(screen.getByRole("button", { name: "Send answer" })));
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toBe("Choose an answer or write your own first.");
    const field = screen.getByRole("textbox", { name: "Or write your own answer" });
    expect(field.getAttribute("aria-invalid")).toBe("true");
    expect(field.getAttribute("aria-describedby")).toBe(alert.id);
    expect(document.activeElement).toBe(field);

    await act(() => fireEvent.change(field, { target: { value: "Later" } }));
    expect(screen.queryByRole("alert")).toBeNull();
    expect(screen.getByText("Sends “Later”")).toBeTruthy();
  });

  test("sends the chosen option to the ask's answer endpoint", async () => {
    const requests: { url: string; body: unknown }[] = [];
    globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
      requests.push({ url: String(input), body: JSON.parse(String(init?.body)) });
      return Response.json(state.ask(withOptions.id));
    }) as typeof fetch;
    await renderInApp(<AnswerForm ask={withOptions} />);
    const option = withOptions.options[0]!;
    await act(() =>
      fireEvent.click(screen.getByRole("radio", { name: new RegExp(option.slice(0, 20)) })),
    );
    await act(() => fireEvent.click(screen.getByRole("button", { name: "Send answer" })));
    await waitFor(() => expect(requests.length).toBe(1));
    expect(requests[0]!.url).toEndWith(`/v1/asks/${withOptions.id}/answer`);
    expect(requests[0]!.body).toEqual({ answer: option });
  });

  test("Cmd or Ctrl with Enter in the field sends the text; Enter alone does not", async () => {
    const requests: unknown[] = [];
    globalThis.fetch = (async (_input: RequestInfo | URL, init?: RequestInit) => {
      requests.push(JSON.parse(String(init?.body)));
      return Response.json(state.ask(withOptions.id));
    }) as typeof fetch;
    await renderInApp(<AnswerForm ask={withOptions} />);
    const field = screen.getByRole("textbox", { name: "Or write your own answer" });
    expect(field.getAttribute("aria-keyshortcuts")).toBe("Meta+Enter Control+Enter");
    await act(() => fireEvent.change(field, { target: { value: "Later" } }));
    await act(() => fireEvent.keyDown(field, { key: "Enter" }));
    expect(requests).toEqual([]);
    await act(() => fireEvent.keyDown(field, { key: "Enter", ctrlKey: true }));
    await waitFor(() => expect(requests).toEqual([{ answer: "Later" }]));
  });

  test("a paste inserts the event's own text at the caret, without reading the clipboard again", async () => {
    await renderInApp(<AnswerForm ask={withOptions} />);
    const field = screen.getByRole<HTMLTextAreaElement>("textbox", {
      name: "Or write your own answer",
    });
    await act(() => fireEvent.change(field, { target: { value: "Ship after review" } }));
    field.setSelectionRange(5, 5);
    // As a dictation app's synthetic ⌘V: the event carries the take. happy-dom lacks
    // execCommand; browsers insert through it, and the field falls back when it fails.
    const commands: unknown[][] = [];
    let inserts = true;
    document.execCommand = (...args: unknown[]) => (commands.push(args), inserts);
    const paste = () => fireEvent.paste(field, { clipboardData: { getData: () => "it today " } });
    expect(await act(paste)).toBe(false);
    expect(commands).toEqual([["insertText", false, "it today "]]);
    expect(field.value).toBe("Ship after review");

    inserts = false;
    expect(await act(paste)).toBe(false);
    expect(field.value).toBe("Ship it today after review");
    expect(screen.getByText("Sends “Ship it today after review”")).toBeTruthy();

    // A paste without text, such as an image, is left to the browser.
    const plain = await act(() => fireEvent.paste(field, { clipboardData: { getData: () => "" } }));
    expect(plain).toBe(true);
    expect(field.value).toBe("Ship it today after review");
  });

  test("a second submit before the first settles sends nothing more", async () => {
    let requests = 0;
    let settle!: () => void;
    globalThis.fetch = (async (_input: RequestInfo | URL, _init?: RequestInit) => {
      requests++;
      await new Promise<void>((resolve) => (settle = resolve));
      return Response.json(state.ask(withOptions.id));
    }) as typeof fetch;
    await renderInApp(<AnswerForm ask={withOptions} />);
    const field = screen.getByRole("textbox", { name: "Or write your own answer" });
    await act(() => fireEvent.change(field, { target: { value: "Later" } }));
    const form = screen.getByRole("button", { name: "Send answer" }).closest("form")!;
    // Two presses of Enter in the same tick, before React renders the pending state.
    await act(async () => {
      fireEvent.submit(form);
      fireEvent.submit(form);
    });
    await waitFor(() => expect(requests).toBe(1));
    await act(async () => settle());
    expect(requests).toBe(1);
  });
});
