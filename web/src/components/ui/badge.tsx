import * as React from "react";
import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "@/lib/utils";
import { Slot } from "radix-ui";

const badgeVariants = cva(
  "inline-flex w-fit shrink-0 items-center justify-center gap-1 overflow-hidden rounded-full border border-transparent px-2 py-0.5 text-xs font-medium whitespace-nowrap transition-[color,box-shadow] focus-visible:border-ring focus-visible:ring-[3px] focus-visible:ring-ring/50 aria-invalid:border-destructive aria-invalid:ring-destructive/20 dark:aria-invalid:ring-destructive/40 [&>svg]:pointer-events-none [&>svg]:size-3",
  {
    variants: {
      variant: {
        default: "bg-primary text-primary-foreground [a&]:hover:bg-primary/90",
        secondary: "bg-secondary text-secondary-foreground [a&]:hover:bg-secondary/90",
        destructive:
          "bg-destructive text-white focus-visible:ring-destructive/20 dark:bg-destructive/60 dark:focus-visible:ring-destructive/40 [a&]:hover:bg-destructive/90",
        outline:
          "border-input bg-card text-subtle-foreground [a&]:hover:bg-accent [a&]:hover:text-accent-foreground",
        ghost: "[a&]:hover:bg-accent [a&]:hover:text-accent-foreground",
        link: "text-primary underline-offset-4 [a&]:hover:underline",
        // agent-board tones: always beside text that says the same thing.
        accent: "border-primary/35 bg-primary-soft text-link",
        warn: "border-amber/35 bg-amber-soft text-amber",
        ok: "border-status-ready/35 bg-status-ready-soft text-status-ready",
        bad: "border-status-blocked/35 bg-status-blocked-soft text-status-blocked",
        review: "border-status-review/35 bg-card text-status-review",
        doing: "border-status-doing/35 bg-card text-status-doing",
        blocked: "border-status-blocked/35 bg-card text-status-blocked",
      },
      shape: {
        pill: "",
        // A tag: squarer and a fixed height, for labels inside rows.
        tag: "h-5.5 rounded-sm px-2 py-0",
      },
    },
    defaultVariants: {
      variant: "default",
      shape: "pill",
    },
  },
);

function Badge({
  className,
  variant = "default",
  shape = "pill",
  asChild = false,
  ...props
}: React.ComponentProps<"span"> & VariantProps<typeof badgeVariants> & { asChild?: boolean }) {
  const Comp = asChild ? Slot.Root : "span";

  return (
    <Comp
      data-slot="badge"
      data-variant={variant}
      className={cn(badgeVariants({ variant, shape }), className)}
      {...props}
    />
  );
}

export { Badge, badgeVariants };
