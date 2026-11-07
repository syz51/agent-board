import * as React from "react";
import { cn } from "@/lib/utils";

// shadcn/ui's Dialog styling on the native <dialog>: showModal() gives focus trapping,
// Escape, inert background and the top layer without a portal. Radix's Dialog is not used
// because its scroll lock injects a <style> element, which the page's CSP refuses.

function Dialog({
  open,
  onOpenChange,
  className,
  children,
  ...props
}: Omit<React.ComponentProps<"dialog">, "open"> & {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const ref = React.useRef<HTMLDialogElement>(null);

  React.useEffect(() => {
    const dialog = ref.current;
    if (!dialog) return;
    if (open && !dialog.open) {
      dialog.showModal();
      // React keeps `autoFocus` to itself, so the dialog marks its default action instead.
      dialog.querySelector<HTMLElement>("[data-autofocus]")?.focus();
    }
    if (!open && dialog.open) dialog.close();
  }, [open]);

  return (
    <dialog
      ref={ref}
      data-slot="dialog"
      className={cn(
        "fixed inset-0 m-auto w-full max-w-[calc(100%-2rem)] rounded-lg border bg-background p-6 text-foreground shadow-overlay backdrop:bg-black/50 open:animate-in open:fade-in-0 open:zoom-in-95 sm:max-w-lg",
        className,
      )}
      onClose={() => onOpenChange(false)}
      onClick={(event) => {
        // A click on the backdrop lands on the dialog element itself.
        if (event.target === ref.current) onOpenChange(false);
      }}
      {...props}
    >
      {open && children}
    </dialog>
  );
}

function DialogHeader({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="dialog-header"
      className={cn("flex flex-col gap-2 text-left", className)}
      {...props}
    />
  );
}

function DialogFooter({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="dialog-footer"
      className={cn("flex flex-col-reverse gap-2 sm:flex-row sm:justify-end", className)}
      {...props}
    />
  );
}

function DialogTitle({ className, ...props }: React.ComponentProps<"h2">) {
  return (
    <h2
      data-slot="dialog-title"
      className={cn("text-base leading-snug font-semibold", className)}
      {...props}
    />
  );
}

function DialogDescription({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="dialog-description"
      className={cn("text-sm text-muted-foreground", className)}
      {...props}
    />
  );
}

export { Dialog, DialogDescription, DialogFooter, DialogHeader, DialogTitle };
