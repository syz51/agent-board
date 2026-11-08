import { useId, useState } from "react";
import { Button } from "@/components/ui/button.tsx";
import { cn } from "@/lib/utils.ts";
import { Markdown } from "./markdown.tsx";

/** Longer than this, or than `CLAMP_LINES` lines, and agent text starts collapsed. */
const CLAMP_CHARS = 360;
const CLAMP_LINES = 6;

export function isLong(text: string): boolean {
  return text.length > CLAMP_CHARS || text.split("\n").length > CLAMP_LINES;
}

/** Agent markdown, collapsed to a few lines when long, with a button to show all of it. */
export function CollapsibleMarkdown({ text }: { text: string }) {
  const [open, setOpen] = useState(false);
  const id = useId();
  if (!isLong(text)) {
    return (
      <div className="mt-0.5 max-w-[72ch]">
        <Markdown text={text} />
      </div>
    );
  }
  return (
    <div className="flex flex-col items-start">
      <div
        id={id}
        className={cn("mt-0.5 max-w-[72ch]", !open && "clamped")}
        data-clamped={open ? undefined : ""}
      >
        <Markdown text={text} />
      </div>
      <Button
        type="button"
        variant="link"
        className="min-h-9 px-0 py-1.5 text-sm underline"
        aria-expanded={open}
        aria-controls={id}
        onClick={() => setOpen(!open)}
      >
        {open ? "Show less" : "Show all"}
      </Button>
    </div>
  );
}
