import { Check, ChevronDown } from 'lucide-react';
import { useCallback, useEffect, useId, useLayoutEffect, useRef, useState } from 'react';
import type { CSSProperties, KeyboardEvent as ReactKeyboardEvent } from 'react';
import { createPortal } from 'react-dom';

export interface SelectOption {
  value: string;
  label: string;
  disabled?: boolean;
}

interface SelectProps {
  value: string;
  options: SelectOption[];
  onChange: (value: string) => void;
  disabled?: boolean;
  id?: string;
  className?: string;
  placeholder?: string;
  'aria-label'?: string;
}

/** 服务环境类型选项：新增服务与外部服务表单共用。 */
export const environmentOptions: SelectOption[] = [
  { value: 'system', label: '系统环境' },
  { value: 'venv', label: 'Python 虚拟环境' },
  { value: 'custom', label: '自定义环境' },
];

/** 菜单与触发按钮之间的间距，以及距离窗口边缘的最小留白（px）。 */
const MENU_GAP = 4;
const VIEWPORT_MARGIN = 8;
const MENU_MAX_HEIGHT = 280;

/**
 * 自定义下拉选择框。
 *
 * 为什么不用原生 <select>：macOS WebKit 会把系统弹出菜单的“当前选中项”对齐到按钮上，
 * 菜单因此盖住按钮；这一行为无法通过 CSS 修改。本组件改为在按钮下方（空间不足时翻到上方）
 * 弹出列表，并通过 portal 渲染到 body，避免被抽屉等 overflow 容器裁剪。
 *
 * 焦点策略：焦点始终停留在触发按钮上，列表项通过 aria-activedescendant 表达当前项，
 * 这样既不会破坏抽屉的焦点陷阱（focusTrap），也能让屏幕阅读器正确播报。
 */
export default function Select({ value, options, onChange, disabled, id, className, placeholder, 'aria-label': ariaLabel }: SelectProps) {
  const [open, setOpen] = useState(false);
  const [activeIndex, setActiveIndex] = useState(-1);
  const [menuStyle, setMenuStyle] = useState<CSSProperties>({});
  const triggerRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const typeahead = useRef({ text: '', timer: 0 });
  const baseId = useId();
  const listboxId = `${baseId}-listbox`;
  const optionId = (index: number) => `${baseId}-option-${index}`;

  const selectedIndex = options.findIndex(option => option.value === value);
  const selected = selectedIndex >= 0 ? options[selectedIndex] : undefined;

  /** 从 start 开始按 step 方向寻找下一个可选项；越界则返回 -1。 */
  const findEnabled = useCallback((start: number, step: 1 | -1) => {
    for (let index = start; index >= 0 && index < options.length; index += step) {
      if (!options[index].disabled) return index;
    }
    return -1;
  }, [options]);

  const openMenu = () => {
    if (disabled) return;
    setActiveIndex(selected && !selected.disabled ? selectedIndex : findEnabled(0, 1));
    setOpen(true);
  };

  const closeMenu = () => setOpen(false);

  const commit = (index: number) => {
    const option = options[index];
    if (!option || option.disabled) return;
    if (option.value !== value) onChange(option.value);
    setOpen(false);
  };

  /**
   * 根据触发按钮位置计算菜单坐标：默认显示在按钮正下方；
   * 当下方空间不足且上方更宽裕时翻转到按钮上方，宽度至少与按钮一致并限制在窗口内。
   */
  const updatePosition = useCallback(() => {
    const trigger = triggerRef.current;
    if (!trigger) return;
    const rect = trigger.getBoundingClientRect();
    const spaceBelow = window.innerHeight - rect.bottom - MENU_GAP - VIEWPORT_MARGIN;
    const spaceAbove = rect.top - MENU_GAP - VIEWPORT_MARGIN;
    const contentHeight = menuRef.current?.scrollHeight ?? MENU_MAX_HEIGHT;
    const desired = Math.min(contentHeight, MENU_MAX_HEIGHT);
    const placeAbove = spaceBelow < desired && spaceAbove > spaceBelow;
    const maxHeight = Math.max(80, Math.min(MENU_MAX_HEIGHT, placeAbove ? spaceAbove : spaceBelow));
    const width = Math.min(Math.max(rect.width, 160), window.innerWidth - VIEWPORT_MARGIN * 2);
    const left = Math.min(Math.max(rect.left, VIEWPORT_MARGIN), window.innerWidth - width - VIEWPORT_MARGIN);
    setMenuStyle({
      left,
      minWidth: width,
      maxWidth: Math.max(width, 360),
      maxHeight,
      ...(placeAbove ? { bottom: window.innerHeight - rect.top + MENU_GAP } : { top: rect.bottom + MENU_GAP }),
    });
  }, []);

  useLayoutEffect(() => {
    if (open) updatePosition();
  }, [open, options.length, updatePosition]);

  // 打开期间：点击外部关闭；窗口失焦关闭；任意容器滚动或窗口缩放时重新定位。
  useEffect(() => {
    if (!open) return;
    const onPointerDown = (event: MouseEvent) => {
      const target = event.target as Node;
      if (triggerRef.current?.contains(target) || menuRef.current?.contains(target)) return;
      setOpen(false);
    };
    const onScroll = (event: Event) => {
      if (menuRef.current?.contains(event.target as Node)) return;
      updatePosition();
    };
    const onBlur = () => setOpen(false);
    document.addEventListener('mousedown', onPointerDown, true);
    window.addEventListener('scroll', onScroll, true);
    window.addEventListener('resize', updatePosition);
    window.addEventListener('blur', onBlur);
    return () => {
      document.removeEventListener('mousedown', onPointerDown, true);
      window.removeEventListener('scroll', onScroll, true);
      window.removeEventListener('resize', updatePosition);
      window.removeEventListener('blur', onBlur);
    };
  }, [open, updatePosition]);

  // 键盘移动当前项时，保证它滚动到可见区域。
  useEffect(() => {
    if (!open || activeIndex < 0) return;
    menuRef.current?.querySelector<HTMLElement>(`[data-index="${activeIndex}"]`)?.scrollIntoView({ block: 'nearest' });
  }, [open, activeIndex]);

  useEffect(() => () => window.clearTimeout(typeahead.current.timer), []);
  useEffect(() => { if (disabled) setOpen(false); }, [disabled]);

  /** 输入字符快速定位：连续输入会拼接成前缀，停顿 600ms 后重置。 */
  const handleTypeahead = (key: string) => {
    const state = typeahead.current;
    window.clearTimeout(state.timer);
    state.text += key.toLowerCase();
    state.timer = window.setTimeout(() => { state.text = ''; }, 600);
    const match = options.findIndex(option => !option.disabled && option.label.toLowerCase().startsWith(state.text));
    if (match < 0) return;
    if (open) setActiveIndex(match);
    else commit(match);
  };

  const onKeyDown = (event: ReactKeyboardEvent<HTMLButtonElement>) => {
    if (disabled) return;
    const { key } = event;
    if (!open) {
      if (key === 'ArrowDown' || key === 'ArrowUp' || key === 'Enter' || key === ' ') {
        event.preventDefault();
        openMenu();
      } else if (key.length === 1 && !event.metaKey && !event.ctrlKey && !event.altKey) {
        handleTypeahead(key);
      }
      return;
    }
    switch (key) {
      case 'ArrowDown': {
        event.preventDefault();
        const next = findEnabled(activeIndex + 1, 1);
        if (next >= 0) setActiveIndex(next);
        break;
      }
      case 'ArrowUp': {
        event.preventDefault();
        const previous = findEnabled(activeIndex - 1, -1);
        if (previous >= 0) setActiveIndex(previous);
        break;
      }
      case 'Home':
        event.preventDefault();
        setActiveIndex(findEnabled(0, 1));
        break;
      case 'End':
        event.preventDefault();
        setActiveIndex(findEnabled(options.length - 1, -1));
        break;
      case 'Enter':
      case ' ':
        event.preventDefault();
        commit(activeIndex);
        break;
      case 'Escape':
        // 必须阻止冒泡：抽屉/对话框在 document 上监听 Escape 关闭自身，这里只应收起菜单。
        event.preventDefault();
        event.stopPropagation();
        closeMenu();
        break;
      case 'Tab':
        closeMenu();
        break;
      default:
        if (key.length === 1 && !event.metaKey && !event.ctrlKey && !event.altKey) handleTypeahead(key);
    }
  };

  return (
    <>
      <button
        ref={triggerRef}
        id={id}
        type="button"
        role="combobox"
        className={`kc-select${open ? ' open' : ''}${className ? ` ${className}` : ''}`}
        aria-label={ariaLabel}
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-controls={open ? listboxId : undefined}
        aria-activedescendant={open && activeIndex >= 0 ? optionId(activeIndex) : undefined}
        disabled={disabled}
        onClick={() => (open ? closeMenu() : openMenu())}
        onKeyDown={onKeyDown}
      >
        <span className={`kc-select-value${selected ? '' : ' placeholder'}`}>{selected?.label ?? placeholder ?? value}</span>
        <ChevronDown size={14} className="kc-select-chevron" aria-hidden="true" />
      </button>
      {open && createPortal(
        <div ref={menuRef} id={listboxId} className="kc-select-menu" role="listbox" aria-label={ariaLabel} style={menuStyle}>
          {options.map((option, index) => (
            <div
              key={`${option.value}-${index}`}
              id={optionId(index)}
              data-index={index}
              role="option"
              aria-selected={index === selectedIndex}
              aria-disabled={option.disabled || undefined}
              className={`kc-select-option${index === activeIndex ? ' active' : ''}${index === selectedIndex ? ' selected' : ''}`}
              // 阻止 mousedown 默认行为，使焦点保留在触发按钮上。
              onMouseDown={event => event.preventDefault()}
              onMouseEnter={() => { if (!option.disabled) setActiveIndex(index); }}
              onClick={() => commit(index)}
            >
              <span>{option.label}</span>
              {index === selectedIndex && <Check size={14} aria-hidden="true" />}
            </div>
          ))}
        </div>,
        document.body,
      )}
    </>
  );
}
