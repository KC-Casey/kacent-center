import { useEffect, useLayoutEffect, useRef } from 'react';
import { FitAddon } from '@xterm/addon-fit';
import { Terminal } from '@xterm/xterm';
import type { TerminalAppearance, TerminalColors } from '../types';
import {
  colorsForTerminalTheme,
  defaultTerminalAppearance,
  resolveTerminalAppearance,
  validateTerminalAppearance,
} from '../terminalAppearance';
import './TerminalAppearanceEditor.css';
import '@xterm/xterm/css/xterm.css';

interface Props {
  value: TerminalAppearance;
  onChange: (value: TerminalAppearance) => void;
  systemTheme: 'dark' | 'light';
}

type ColorKey = keyof TerminalColors;

const COLOR_LABELS: Record<ColorKey, string> = {
  background: '背景',
  foreground: '前景文字',
  cursor: '光标',
  selectionBackground: '选中背景',
  black: '黑色',
  red: '红色',
  green: '绿色',
  yellow: '黄色',
  blue: '蓝色',
  magenta: '洋红色',
  cyan: '青色',
  white: '白色',
  brightBlack: '亮黑色',
  brightRed: '亮红色',
  brightGreen: '亮绿色',
  brightYellow: '亮黄色',
  brightBlue: '亮蓝色',
  brightMagenta: '亮洋红色',
  brightCyan: '亮青色',
  brightWhite: '亮白色',
};

const BASE_COLOR_KEYS = ['background', 'foreground', 'cursor', 'selectionBackground'] as const satisfies readonly ColorKey[];
const ANSI_COLOR_KEYS = [
  'black', 'red', 'green', 'yellow', 'blue', 'magenta', 'cyan', 'white',
  'brightBlack', 'brightRed', 'brightGreen', 'brightYellow', 'brightBlue', 'brightMagenta', 'brightCyan', 'brightWhite',
] as const satisfies readonly ColorKey[];

const PRESET_OPTIONS: Array<{ value: TerminalAppearance['theme']; label: string; detail: string }> = [
  { value: 'system', label: '跟随系统', detail: '随系统深浅色外观切换' },
  { value: 'dark', label: '深色', detail: 'Kacent 深色终端配色' },
  { value: 'light', label: '浅色', detail: '适合明亮环境的配色' },
  { value: 'dracula', label: 'Dracula', detail: '紫色高对比配色' },
  { value: 'solarizedDark', label: 'Solarized Dark', detail: '低对比深色配色' },
  { value: 'custom', label: '自定义', detail: '使用下方颜色设置' },
];

const PREVIEW_CONTENT = [
  '\x1b[1m终端外观预览\x1b[0m  Kacent Center',
  '中文输出：你好，世界！  $ echo "hello terminal"',
  '\x1b[30m黑\x1b[0m \x1b[31m红\x1b[0m \x1b[32m绿\x1b[0m \x1b[33m黄\x1b[0m \x1b[34m蓝\x1b[0m \x1b[35m洋红\x1b[0m \x1b[36m青\x1b[0m \x1b[37m白\x1b[0m',
  '\x1b[90m亮黑\x1b[0m \x1b[91m亮红\x1b[0m \x1b[92m亮绿\x1b[0m \x1b[93m亮黄\x1b[0m \x1b[94m亮蓝\x1b[0m \x1b[95m亮洋红\x1b[0m \x1b[96m亮青\x1b[0m \x1b[97m亮白\x1b[0m',
  '\x1b[2m工具自身配色可能覆盖终端背景。\x1b[0m',
].join('\r\n');

function isHexColor(value: string): boolean {
  return /^#[0-9a-f]{6}$/i.test(value);
}

function ColorControl({ colorKey, value, onChange }: { colorKey: ColorKey; value: string; onChange: (value: string) => void }) {
  const id = `terminal-color-${colorKey}`;
  return <label className="kc-terminal-color-control" htmlFor={id}>
    <span>{COLOR_LABELS[colorKey]}</span>
    <span className="kc-terminal-color-inputs">
      <input
        type="color"
        value={isHexColor(value) ? value : '#000000'}
        onChange={event => onChange(event.target.value.toUpperCase())}
        aria-label={`${COLOR_LABELS[colorKey]}颜色选择器`}
      />
      <input
        id={id}
        className="kc-terminal-color-text"
        type="text"
        value={value}
        onChange={event => onChange(event.target.value)}
        inputMode="text"
        spellCheck={false}
        aria-label={`${COLOR_LABELS[colorKey]}十六进制颜色`}
        placeholder="#RRGGBB"
      />
    </span>
  </label>;
}

/**
 * 终端外观抽屉内容只维护“草稿回调”，保存、取消和后端写入由根设置抽屉负责。
 * 预览使用独立 xterm 实例，不创建 PTY，也不会订阅任何真实会话。
 */
export default function TerminalAppearanceEditor({ value, onChange, systemTheme }: Props) {
  const previewHost = useRef<HTMLDivElement>(null);
  const previewTerminal = useRef<Terminal | null>(null);
  const previewFit = useRef<FitAddon | null>(null);

  useLayoutEffect(() => {
    const host = previewHost.current;
    if (!host) return;
    const initial = resolveTerminalAppearance(value, systemTheme);
    const terminal = new Terminal({
      fontFamily: initial.fontFamily,
      fontSize: initial.fontSize,
      lineHeight: initial.lineHeight,
      cursorStyle: initial.cursorStyle,
      cursorBlink: initial.cursorBlink,
      theme: initial.colors,
      disableStdin: true,
      convertEol: false,
      scrollback: 64,
      screenReaderMode: true,
      allowProposedApi: false,
    });
    const fit = new FitAddon();
    terminal.loadAddon(fit);
    terminal.open(host);
    terminal.write(PREVIEW_CONTENT);
    previewTerminal.current = terminal;
    previewFit.current = fit;
    const resize = () => {
      if (host.clientWidth >= 20 && host.clientHeight >= 20) fit.fit();
    };
    const observer = new ResizeObserver(resize);
    observer.observe(host);
    resize();
    return () => {
      observer.disconnect();
      previewTerminal.current = null;
      previewFit.current = null;
      fit.dispose();
      terminal.dispose();
    };
  }, []);

  useEffect(() => {
    const terminal = previewTerminal.current;
    if (!terminal) return;
    const resolved = resolveTerminalAppearance(value, systemTheme);
    terminal.options.theme = resolved.colors;
    terminal.options.fontFamily = resolved.fontFamily;
    terminal.options.fontSize = resolved.fontSize;
    terminal.options.lineHeight = resolved.lineHeight;
    terminal.options.cursorStyle = resolved.cursorStyle;
    terminal.options.cursorBlink = resolved.cursorBlink;
    previewFit.current?.fit();
  }, [value, systemTheme]);

  const resolvedAppearance = resolveTerminalAppearance(value, systemTheme);
  const patch = (changes: Partial<TerminalAppearance>) => onChange({ ...value, ...changes, colors: { ...value.colors } });
  const patchColors = (changes: Partial<TerminalColors>) => {
    // 预设颜色只读；第一次改色时以当前实际预设为起点，避免系统浅色下退回深色默认值。
    const colors = value.theme === 'custom' ? value.colors : resolvedAppearance.colors;
    onChange({ ...value, theme: 'custom', colors: { ...colors, ...changes } });
  };
  const validationMessage = validateTerminalAppearance(value);
  const editableColors = value.theme === 'custom' ? value.colors : resolvedAppearance.colors;

  const selectPreset = (theme: TerminalAppearance['theme']) => {
    const colors = theme === 'custom'
      ? { ...resolvedAppearance.colors }
      : colorsForTerminalTheme(theme === 'system' ? 'system' : theme, systemTheme);
    onChange({ ...value, theme, colors });
  };

  return <div className="kc-terminal-appearance-editor">
    <div className="kc-terminal-appearance-intro">
      <h3>终端外观</h3>
      <p>调整终端字体、光标和配色。点击保存后才会应用到已有会话。</p>
      <p className="kc-terminal-appearance-note">使用独立配色的工具仍遵循其自身主题设置。</p>
    </div>

    <section className="kc-terminal-appearance-section" aria-labelledby="terminal-appearance-presets">
      <div className="kc-terminal-appearance-section-heading"><h3 id="terminal-appearance-presets">预设</h3><button type="button" className="kc-terminal-reset" onClick={() => onChange(defaultTerminalAppearance())}>恢复默认</button></div>
      <div className="kc-terminal-preset-list" role="radiogroup" aria-label="终端配色预设">
        {PRESET_OPTIONS.map(option => <button
          type="button"
          role="radio"
          aria-checked={value.theme === option.value}
          className={`kc-terminal-preset ${value.theme === option.value ? 'selected' : ''}`}
          key={option.value}
          onClick={() => selectPreset(option.value)}
        ><span className="kc-terminal-preset-copy"><strong>{option.label}</strong><small>{option.detail}</small></span>{value.theme === option.value && <span aria-hidden="true">✓</span>}</button>)}
      </div>
    </section>

    <section className="kc-terminal-appearance-section" aria-labelledby="terminal-appearance-text">
      <h3 id="terminal-appearance-text">文字与光标</h3>
      <div className="kc-terminal-form-grid">
        <label className="kc-terminal-form-field kc-terminal-font-field" htmlFor="terminal-font-family"><span>字体</span><input id="terminal-font-family" type="text" value={value.fontFamily} onChange={event => patch({ fontFamily: event.target.value })} placeholder="例如：SFMono-Regular, monospace" /></label>
        <label className="kc-terminal-form-field" htmlFor="terminal-font-size"><span>字号（9–32）</span><input id="terminal-font-size" type="number" min={9} max={32} step={1} value={Number.isFinite(value.fontSize) ? value.fontSize : ''} onChange={event => patch({ fontSize: Number(event.target.value) })} /></label>
        <label className="kc-terminal-form-field" htmlFor="terminal-line-height"><span>行高（1–2）</span><input id="terminal-line-height" type="number" min={1} max={2} step={0.05} value={Number.isFinite(value.lineHeight) ? value.lineHeight : ''} onChange={event => patch({ lineHeight: Number(event.target.value) })} /></label>
        <label className="kc-terminal-form-field" htmlFor="terminal-cursor-style"><span>光标样式</span><select id="terminal-cursor-style" value={value.cursorStyle} onChange={event => patch({ cursorStyle: event.target.value as TerminalAppearance['cursorStyle'] })}><option value="block">方块</option><option value="underline">下划线</option><option value="bar">竖线</option></select></label>
        <label className="kc-terminal-checkbox-field" htmlFor="terminal-cursor-blink"><input id="terminal-cursor-blink" type="checkbox" checked={value.cursorBlink} onChange={event => patch({ cursorBlink: event.target.checked })} /><span>光标闪烁</span></label>
      </div>
    </section>

    <section className="kc-terminal-appearance-section" aria-labelledby="terminal-appearance-colors">
      <h3 id="terminal-appearance-colors">界面颜色</h3>
      <div className="kc-terminal-color-grid">{BASE_COLOR_KEYS.map(colorKey => <ColorControl key={colorKey} colorKey={colorKey} value={editableColors[colorKey]} onChange={color => patchColors({ [colorKey]: color })} />)}</div>
      <details className="kc-terminal-ansi-details">
        <summary>16 色 ANSI 调色板</summary>
        <div className="kc-terminal-color-grid">{ANSI_COLOR_KEYS.map(colorKey => <ColorControl key={colorKey} colorKey={colorKey} value={editableColors[colorKey]} onChange={color => patchColors({ [colorKey]: color })} />)}</div>
      </details>
      {validationMessage && <p className="kc-terminal-validation" role="alert">{validationMessage}</p>}
    </section>

    <section className="kc-terminal-appearance-section" aria-labelledby="terminal-appearance-preview">
      <h3 id="terminal-appearance-preview">预览</h3>
      <div className="kc-terminal-preview" ref={previewHost} style={{ backgroundColor: resolvedAppearance.colors.background }} aria-label="终端外观预览" />
    </section>
  </div>;
}
