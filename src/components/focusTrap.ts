import type { RefObject } from 'react';

/** 让抽屉保持模态键盘路径，并在关闭后把焦点还给打开抽屉的控件。 */
export function trapDialogFocus<T extends HTMLElement>(dialogRef: RefObject<T | null>): () => void {
  const dialog = dialogRef.current;
  if (!dialog) return () => undefined;

  const previousFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  const focusable = () => Array.from(dialog.querySelectorAll<HTMLElement>(
    'button:not(:disabled), input:not(:disabled), textarea:not(:disabled), select:not(:disabled), [tabindex]:not([tabindex="-1"])',
  ));
  (dialog.querySelector<HTMLElement>('[autofocus]') ?? focusable()[0])?.focus();

  const onKeyDown = (event: KeyboardEvent) => {
    if (event.key !== 'Tab') return;
    const items = focusable();
    if (!items.length) {
      event.preventDefault();
      dialog.focus();
      return;
    }
    if (event.shiftKey && document.activeElement === items[0]) {
      event.preventDefault();
      items[items.length - 1].focus();
    } else if (!event.shiftKey && document.activeElement === items[items.length - 1]) {
      event.preventDefault();
      items[0].focus();
    }
  };

  document.addEventListener('keydown', onKeyDown);
  return () => {
    document.removeEventListener('keydown', onKeyDown);
    previousFocus?.focus();
  };
}
