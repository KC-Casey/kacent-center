import type { TerminalAppearance, TerminalColors } from './types';

/**
 * 终端使用的 16 色 ANSI 调色板。
 * 基础 8 色沿用现有深色终端主题，亮色变体只在终端明确请求 bright
 * 颜色时使用；这样旧会话的普通输出仍能保持原来的视觉效果。
 */
export const DEFAULT_DARK_COLORS: TerminalColors = {
  background: '#11171c',
  foreground: '#d9e3ee',
  cursor: '#e6edf7',
  selectionBackground: '#365779',
  black: '#25313d',
  red: '#f47983',
  green: '#73d59a',
  yellow: '#e4c979',
  blue: '#76a8ff',
  magenta: '#c8a5eb',
  cyan: '#80d7d1',
  white: '#d9e3ee',
  brightBlack: '#5f6f7e',
  brightRed: '#ff9ca4',
  brightGreen: '#a0edbf',
  brightYellow: '#f5e8a8',
  brightBlue: '#a8c8ff',
  brightMagenta: '#e4d2ff',
  brightCyan: '#a9f0eb',
  brightWhite: '#ffffff',
};

const LIGHT_COLORS: TerminalColors = {
  background: '#ffffff',
  foreground: '#253142',
  cursor: '#253142',
  selectionBackground: '#cbdfff',
  black: '#253142',
  red: '#c42b46',
  green: '#1e7e4a',
  yellow: '#946300',
  blue: '#286ed5',
  magenta: '#8a4caf',
  cyan: '#197b87',
  white: '#eef2f7',
  brightBlack: '#5c6b7d',
  brightRed: '#e05268',
  brightGreen: '#36a86e',
  brightYellow: '#b17b00',
  brightBlue: '#4d8ce1',
  brightMagenta: '#a66bc6',
  brightCyan: '#35a5b0',
  brightWhite: '#ffffff',
};

const DRACULA_COLORS: TerminalColors = {
  background: '#282a36',
  foreground: '#f8f8f2',
  cursor: '#f8f8f2',
  selectionBackground: '#44475a',
  black: '#21222c',
  red: '#ff5555',
  green: '#50fa7b',
  yellow: '#f1fa8c',
  blue: '#bd93f9',
  magenta: '#ff79c6',
  cyan: '#8be9fd',
  white: '#f8f8f2',
  brightBlack: '#6272a4',
  brightRed: '#ff6e6e',
  brightGreen: '#69ff94',
  brightYellow: '#ffffa5',
  brightBlue: '#d6acff',
  brightMagenta: '#ff92df',
  brightCyan: '#a4ffff',
  brightWhite: '#ffffff',
};

const SOLARIZED_DARK_COLORS: TerminalColors = {
  background: '#002b36',
  foreground: '#839496',
  cursor: '#839496',
  selectionBackground: '#073642',
  black: '#073642',
  red: '#dc322f',
  green: '#859900',
  yellow: '#b58900',
  blue: '#268bd2',
  magenta: '#d33682',
  cyan: '#2aa198',
  white: '#eee8d5',
  brightBlack: '#586e75',
  brightRed: '#cb4b16',
  brightGreen: '#b58900',
  brightYellow: '#657b83',
  brightBlue: '#839496',
  brightMagenta: '#6c71c4',
  brightCyan: '#93a1a1',
  brightWhite: '#fdf6e3',
};

const DEFAULT_FONT_FAMILY = '"SFMono-Regular", Consolas, "Liberation Mono", monospace';
const THEMES = ['system', 'dark', 'light', 'dracula', 'solarizedDark', 'custom'] as const;
const CURSOR_STYLES = ['block', 'underline', 'bar'] as const;
const COLOR_KEYS = [
  'background', 'foreground', 'cursor', 'selectionBackground',
  'black', 'red', 'green', 'yellow', 'blue', 'magenta', 'cyan', 'white',
  'brightBlack', 'brightRed', 'brightGreen', 'brightYellow', 'brightBlue', 'brightMagenta', 'brightCyan', 'brightWhite',
] as const satisfies readonly (keyof TerminalColors)[];
const COLOR_FIELD_LABELS: Record<keyof TerminalColors, string> = {
  background: '背景', foreground: '前景文字', cursor: '光标', selectionBackground: '选中背景',
  black: '黑色', red: '红色', green: '绿色', yellow: '黄色', blue: '蓝色', magenta: '洋红色', cyan: '青色', white: '白色',
  brightBlack: '亮黑色', brightRed: '亮红色', brightGreen: '亮绿色', brightYellow: '亮黄色', brightBlue: '亮蓝色', brightMagenta: '亮洋红色', brightCyan: '亮青色', brightWhite: '亮白色',
};

function copyColors(colors: TerminalColors): TerminalColors {
  return { ...colors };
}

function copyAppearance(appearance: TerminalAppearance): TerminalAppearance {
  return { ...appearance, colors: copyColors(appearance.colors) };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isHexColor(value: unknown): value is string {
  return typeof value === 'string' && /^#[0-9a-f]{6}$/i.test(value);
}

function validTheme(value: unknown): TerminalAppearance['theme'] {
  return typeof value === 'string' && (THEMES as readonly string[]).includes(value)
    ? value as TerminalAppearance['theme']
    : 'system';
}

function validCursorStyle(value: unknown): TerminalAppearance['cursorStyle'] {
  return typeof value === 'string' && (CURSOR_STYLES as readonly string[]).includes(value)
    ? value as TerminalAppearance['cursorStyle']
    : 'block';
}

function finiteOr(value: unknown, fallback: number): number {
  return typeof value === 'number' && Number.isFinite(value) ? value : fallback;
}

function clamp(value: number, minimum: number, maximum: number): number {
  return Math.min(maximum, Math.max(minimum, value));
}

function normalizeColors(value: unknown): TerminalColors {
  const source = isRecord(value) ? value : {};
  const fallback = DEFAULT_DARK_COLORS;
  const colors = {} as TerminalColors;
  COLOR_KEYS.forEach(key => {
    const color = source[key];
    // 只接受完整的 #RRGGBB，避免 xterm 和 Rust 对同一份配置产生不同解释。
    colors[key] = isHexColor(color) ? color.toUpperCase() : fallback[key];
  });
  return colors;
}

function isValidFontFamily(value: unknown): value is string {
  return typeof value === 'string' && !!value.trim() && Array.from(value).length <= 300 && !/[\u0000-\u001f\u007f-\u009f]/.test(value);
}

/** 返回一份可安全保存或编辑的默认终端外观，不共享内部颜色对象。 */
export function defaultTerminalAppearance(): TerminalAppearance {
  return {
    theme: 'system',
    fontFamily: DEFAULT_FONT_FAMILY,
    fontSize: 13,
    lineHeight: 1.45,
    cursorStyle: 'block',
    cursorBlink: true,
    colors: copyColors(DEFAULT_DARK_COLORS),
  };
}

/**
 * 读取旧缓存或后端返回的外观配置。
 * 缺失字段和非法值只在这里补齐；编辑器草稿本身不会调用本函数，以便
 * 校验时能明确告诉用户“字体为空”或“颜色格式错误”。
 */
export function normalizeTerminalAppearance(value: unknown): TerminalAppearance {
  const source = isRecord(value) ? value : {};
  const fallback = defaultTerminalAppearance();
  const fontFamily = isValidFontFamily(source.fontFamily)
    ? source.fontFamily
    : fallback.fontFamily;
  return {
    theme: validTheme(source.theme),
    fontFamily,
    fontSize: clamp(finiteOr(source.fontSize, fallback.fontSize), 9, 32),
    lineHeight: clamp(finiteOr(source.lineHeight, fallback.lineHeight), 1, 2),
    cursorStyle: validCursorStyle(source.cursorStyle),
    cursorBlink: typeof source.cursorBlink === 'boolean' ? source.cursorBlink : fallback.cursorBlink,
    colors: normalizeColors(source.colors),
  };
}

/** 返回预设的纯颜色数据，调用方可据此把预设写入自定义草稿。 */
export function colorsForTerminalTheme(theme: Exclude<TerminalAppearance['theme'], 'system' | 'custom'> | 'system', systemTheme: 'dark' | 'light'): TerminalColors {
  if (theme === 'system') return copyColors(systemTheme === 'dark' ? DEFAULT_DARK_COLORS : LIGHT_COLORS);
  if (theme === 'dark') return copyColors(DEFAULT_DARK_COLORS);
  if (theme === 'light') return copyColors(LIGHT_COLORS);
  if (theme === 'dracula') return copyColors(DRACULA_COLORS);
  return copyColors(SOLARIZED_DARK_COLORS);
}

/** 将“跟随系统”解析为当前实际主题，供 xterm 实例与只读预览共用。 */
export function resolveTerminalAppearance(appearance: TerminalAppearance, systemTheme: 'dark' | 'light'): TerminalAppearance {
  const normalized = normalizeTerminalAppearance(appearance);
  const resolvedTheme = normalized.theme === 'system' ? systemTheme : normalized.theme;
  let colors: TerminalColors;
  if (normalized.theme === 'custom') {
    colors = normalized.colors;
  } else {
    const presetTheme = normalized.theme === 'system' ? 'system' : normalized.theme;
    colors = colorsForTerminalTheme(presetTheme, systemTheme);
  }
  return { ...copyAppearance(normalized), theme: resolvedTheme as TerminalAppearance['theme'], colors: copyColors(colors) };
}

/**
 * 只校验，不补默认值。设置抽屉在保存前调用，确保用户能看到具体错误。
 * 返回 null 表示可以保存；返回中文错误表示应保留抽屉和草稿。
 */
export function validateTerminalAppearance(value: unknown): string | null {
  if (!isRecord(value)) return '终端外观配置无效。';
  if (!THEMES.includes(value.theme as TerminalAppearance['theme'])) return '终端主题预设无效。';
  if (typeof value.fontFamily !== 'string' || !value.fontFamily.trim()) return '字体不能为空。';
  if (Array.from(value.fontFamily).length > 300) return '字体设置不能超过 300 个字符。';
  if (/[\u0000-\u001f\u007f-\u009f]/.test(value.fontFamily)) return '字体设置不能包含控制字符。';
  if (typeof value.fontSize !== 'number' || !Number.isFinite(value.fontSize) || value.fontSize < 9 || value.fontSize > 32) return '字号必须在 9 到 32 之间。';
  if (typeof value.lineHeight !== 'number' || !Number.isFinite(value.lineHeight) || value.lineHeight < 1 || value.lineHeight > 2) return '行高必须在 1 到 2 之间。';
  if (!CURSOR_STYLES.includes(value.cursorStyle as TerminalAppearance['cursorStyle'])) return '光标样式无效。';
  if (typeof value.cursorBlink !== 'boolean') return '光标闪烁设置无效。';
  if (!isRecord(value.colors)) return '终端颜色配置无效。';
  for (const key of COLOR_KEYS) {
    if (!isHexColor(value.colors[key])) return `颜色“${COLOR_FIELD_LABELS[key]}”必须是 #RRGGBB 格式。`;
  }
  return null;
}
