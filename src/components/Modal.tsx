import { useEffect, useId, useRef } from 'react';
import { AlertTriangle, Info } from 'lucide-react';

export interface ModalAction {
  label: string;
  onClick: () => void;
  variant?: 'primary' | 'danger' | 'secondary';
  disabled?: boolean;
  autoFocus?: boolean;
}

interface ModalProps {
  title: string;
  children: React.ReactNode;
  actions: ModalAction[];
  onCancel?: () => void;
  tone?: 'normal' | 'danger';
}

/**
 * 应用内确认对话框：统一处理焦点进入、Tab 限制与 Escape，保证高风险动作
 * 在 Tauri 窗口中也能清楚确认，并让键盘和辅助技术用户有一致的操作路径。
 */
export default function Modal({ title, children, actions, onCancel, tone = 'normal' }: ModalProps) {
  const dialogRef = useRef<HTMLDivElement>(null);
  const titleId = useId();

  useEffect(() => {
    const previousFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const firstFocusable = dialogRef.current?.querySelector<HTMLElement>('[data-autofocus="true"]')
      ?? dialogRef.current?.querySelector<HTMLElement>('button:not(:disabled), input:not(:disabled), textarea:not(:disabled)');
    firstFocusable?.focus();

    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.stopPropagation();
        if (onCancel) onCancel();
        return;
      }
      if (event.key !== 'Tab' || !dialogRef.current) return;

      // 将 Tab 焦点限制在当前对话框，避免用户在确认时误操作背景页面。
      const items = Array.from(dialogRef.current.querySelectorAll<HTMLElement>(
        'button:not(:disabled), input:not(:disabled), textarea:not(:disabled), select:not(:disabled), [tabindex]:not([tabindex="-1"])',
      ));
      if (items.length === 0) return;
      const first = items[0];
      const last = items[items.length - 1];
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    };

    document.addEventListener('keydown', onKeyDown);
    return () => {
      document.removeEventListener('keydown', onKeyDown);
      previousFocus?.focus();
    };
  }, [onCancel]);

  return (
    <div className="modal-scrim" onMouseDown={(event) => event.target === event.currentTarget && onCancel?.()}>
      <div
        ref={dialogRef}
        className="modal-card"
        role="alertdialog"
        aria-modal="true"
        aria-labelledby={titleId}
        onMouseDown={(event) => event.stopPropagation()}
      >
        <div className={`modal-icon ${tone === 'danger' ? 'is-danger' : ''}`} aria-hidden="true">
          {tone === 'danger' ? <AlertTriangle size={19} /> : <Info size={19} />}
        </div>
        <div className="modal-copy">
          <h2 id={titleId}>{title}</h2>
          <div className="modal-message">{children}</div>
          <div className="modal-actions">
            {actions.map((action) => (
              <button
                key={action.label}
                type="button"
                className={`button ${action.variant ?? 'secondary'}`}
                disabled={action.disabled}
                data-autofocus={action.autoFocus ? 'true' : undefined}
                onClick={action.onClick}
              >
                {action.label}
              </button>
            ))}
          </div>
        </div>
      </div>
    </div>
  );
}
