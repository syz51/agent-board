import * as React from "react";
import { cn } from "@/lib/utils";
import { Switch as SwitchPrimitive } from "radix-ui";

// shadcn/ui's Switch, with its label inside the button: the whole row is the tap target
// (32px tall) and the label names the switch. "On" or "Off" is written beside the track,
// so the state reads without colour or the thumb's position; screen readers get the
// state from the switch role instead.
function Switch({
  className,
  children,
  ...props
}: React.ComponentProps<typeof SwitchPrimitive.Root>) {
  return (
    <SwitchPrimitive.Root
      data-slot="switch"
      className={cn(
        "group/switch inline-flex min-h-8 shrink-0 cursor-pointer items-center gap-2 rounded-sm text-meta font-[550] text-subtle-foreground outline-none disabled:cursor-not-allowed disabled:opacity-60",
        className,
      )}
      {...props}
    >
      {children}
      <span
        data-slot="switch-track"
        className="relative h-[18px] w-8 flex-none rounded-full bg-input transition-colors group-focus-visible/switch:ring-[3px] group-focus-visible/switch:ring-ring/50 group-data-[state=checked]/switch:bg-status-ready"
        aria-hidden="true"
      >
        <SwitchPrimitive.Thumb
          data-slot="switch-thumb"
          className="pointer-events-none absolute top-0.5 left-0.5 block size-3.5 rounded-full bg-white shadow-[0_1px_2px_rgb(0_0_0/0.3)] transition-transform data-[state=checked]:translate-x-3.5"
        />
      </span>
      <span
        data-slot="switch-state"
        className="min-w-[3ch] text-left tabular-nums group-data-[state=checked]/switch:text-foreground"
        aria-hidden="true"
      >
        <span className="group-data-[state=checked]/switch:hidden">Off</span>
        <span className="hidden group-data-[state=checked]/switch:inline">On</span>
      </span>
    </SwitchPrimitive.Root>
  );
}

export { Switch };
