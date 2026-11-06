import type { ComponentChildren } from "preact";
import { useEffect, useRef } from "preact/hooks";

/** A modal confirmation on the native `<dialog>`: focus, Escape and the backdrop are built in. */
export function ConfirmDialog(
  { open, title, children, confirmLabel, onConfirm, onCancel }: {
    open: boolean;
    title: string;
    children: ComponentChildren;
    confirmLabel: string;
    onConfirm: () => void;
    onCancel: () => void;
  },
) {
  const ref = useRef<HTMLDialogElement>(null);

  useEffect(() => {
    const dialog = ref.current;
    if (!dialog) return;
    if (open && !dialog.open) dialog.showModal();
    if (!open && dialog.open) dialog.close();
  }, [open]);

  return (
    <dialog
      ref={ref}
      class="dialog"
      aria-labelledby="confirm-title"
      onClose={onCancel}
      onClick={(event) => {
        // A click on the backdrop lands on the dialog element itself.
        if (event.target === ref.current) onCancel();
      }}
    >
      <form
        method="dialog"
        class="dialog-body"
        onSubmit={(event) => {
          event.preventDefault();
          onConfirm();
        }}
      >
        <h2 id="confirm-title">{title}</h2>
        <div class="dialog-text">{children}</div>
        <div class="dialog-actions">
          <button type="button" class="button" onClick={onCancel}>Cancel</button>
          <button type="submit" class="button button-primary" autofocus>{confirmLabel}</button>
        </div>
      </form>
    </dialog>
  );
}
