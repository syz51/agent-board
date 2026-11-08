// Toolbar controls shared by the board, agents and events pages: a search box, a labelled
// select and a checkbox toggle, all 34px tall on the card surface.

import { type ReactNode, useState } from "react";
import { Input } from "@/components/ui/input.tsx";
import { NativeSelect } from "@/components/ui/native-select.tsx";
import { cn } from "@/lib/utils.ts";
import { Icon } from "./icons.tsx";

/**
 * A search box. It keeps its own text so typing stays immediate, and reports each change.
 * The URL catches up a moment later, sometimes one keystroke at a time: values it reports
 * that this box sent are echoes and leave the text alone; any other value (back, a link)
 * replaces it.
 */
export function SearchField({
  value,
  onChange,
  label,
  placeholder,
  className,
}: {
  value: string;
  onChange: (value: string) => void;
  label: string;
  placeholder: string;
  className?: string;
}) {
  const [draft, setDraft] = useState(value);
  const [seen, setSeen] = useState(value);
  const [sent, setSent] = useState<readonly string[]>([]);
  if (value !== seen) {
    setSeen(value);
    if (value === draft) setSent([]);
    else if (!sent.includes(value)) {
      setDraft(value);
      setSent([]);
    }
  }
  return (
    <label
      className={cn(
        "relative flex flex-[1_1_240px] items-center text-muted-foreground has-disabled:opacity-70",
        className,
      )}
    >
      <Icon name="search" className="pointer-events-none absolute left-2.5" />
      <span className="sr-only">{label}</span>
      <Input
        type="search"
        className="h-[34px] bg-card pl-8 text-foreground shadow-raise disabled:opacity-100 dark:bg-card"
        value={draft}
        placeholder={placeholder}
        onChange={(event) => {
          const next = event.currentTarget.value;
          setDraft(next);
          setSent((list) => [...list, next]);
          onChange(next);
        }}
      />
    </label>
  );
}

/** A select with its label inside the same box: "Priority Any". */
export function FilterSelect({
  label,
  value,
  onChange,
  className,
  children,
}: {
  label: string;
  value: string;
  onChange: (value: string) => void;
  className?: string;
  children: ReactNode;
}) {
  return (
    <label
      className={cn(
        "flex h-[34px] min-w-0 items-center rounded-md border border-input bg-card shadow-raise focus-within:border-ring focus-within:outline-2 focus-within:-outline-offset-1 focus-within:outline-ring hover:border-muted-foreground has-disabled:pointer-events-none has-disabled:opacity-70",
        className,
      )}
    >
      <span className="pl-2.5 text-meta whitespace-nowrap text-muted-foreground">{label}</span>
      <NativeSelect
        wrapperClassName="min-w-0 flex-1 [&>svg]:right-2 [&>svg]:size-3.5"
        className="h-full min-h-0 cursor-pointer truncate border-0 bg-transparent py-0 pr-6 pl-1.5 text-meta font-[550] text-subtle-foreground shadow-none focus-visible:ring-0 focus-visible:outline-none dark:bg-transparent dark:hover:bg-transparent"
        value={value}
        onChange={(event) => onChange(event.currentTarget.value)}
      >
        {children}
      </NativeSelect>
    </label>
  );
}

/** A checkbox in a bordered box that turns accent when checked. */
export function FilterToggle({
  checked,
  onChange,
  className,
  children,
}: {
  checked: boolean;
  onChange: (checked: boolean) => void;
  className?: string;
  children: ReactNode;
}) {
  return (
    <label
      className={cn(
        "flex h-[34px] cursor-pointer items-center gap-2 rounded-md border border-input bg-card pr-3 pl-2.5 text-meta font-medium whitespace-nowrap shadow-raise has-checked:border-signal/45 has-checked:bg-primary-soft has-checked:text-link has-focus-visible:outline-2 has-focus-visible:outline-offset-2 has-focus-visible:outline-ring has-disabled:pointer-events-none has-disabled:opacity-70 max-md:pr-2.5 max-md:pl-2 [&_input]:outline-none",
        className,
      )}
    >
      {/* Native, like every choice here: Radix adds a hidden duplicate input inside forms. */}
      <input
        type="checkbox"
        className="m-0 size-4 flex-none cursor-pointer accent-primary"
        checked={checked}
        onChange={(event) => onChange(event.currentTarget.checked)}
      />
      {children}
    </label>
  );
}
