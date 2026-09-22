import { Channel, invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import type { TerminalAppearance, TerminalEvent, TerminalSession } from './types';
import { resolveTerminalAppearance } from './terminalAppearance';
import { isDesktop } from './api';
import '@xterm/xterm/css/xterm.css';

type PendingInput = { data: string; bytes: number };
type TerminalEntry = {
  terminal: Terminal; fit: FitAddon; host: HTMLDivElement; channel?: Channel<TerminalEvent>;
  generation: number; subscription: 'idle' | 'connecting' | 'connected' | 'failed';
  acknowledged: number; ackConfirmed: number; pendingSeqs: Set<number>;
  ackTimer?: ReturnType<typeof setTimeout>; ackInFlight: boolean; ackWarningShown: boolean;
  disposed: boolean; inputQueue: PendingInput[]; inputQueuedBytes: number;
  inputSending: boolean; inputOverflowWarned: boolean; session: TerminalSession;
};
const entries = new Map<string, TerminalEntry>();
const MAX_QUEUED_INPUT_BYTES = 1024 * 1024;
const ACK_RETRY_DELAYS = [100, 300, 700] as const;
const inputEncoder = new TextEncoder();

function scheduleAck(entry: TerminalEntry, delay = 16) {
  if (entry.disposed || entry.acknowledged <= entry.ackConfirmed || entry.ackTimer || entry.ackInFlight) return;
  entry.ackTimer = setTimeout(() => {
    entry.ackTimer = undefined;
    void sendAck(entry, 0);
  }, delay);
}

async function sendAck(entry: TerminalEntry, retry: number) {
  if (entry.disposed || entry.ackInFlight || entry.acknowledged <= entry.ackConfirmed) return;
  const seq = entry.acknowledged;
  entry.ackInFlight = true;
  let retryScheduled = false;
  let retryExhausted = false;
  try {
    await invoke<void>('ack_terminal', { sessionId: entry.session.id, seq });
    if (entry.disposed) return;
    entry.ackConfirmed = Math.max(entry.ackConfirmed, seq);
    entry.ackWarningShown = false;
  } catch (error) {
    if (entry.disposed) return;
    const delay = ACK_RETRY_DELAYS[retry];
    if (delay !== undefined) {
      retryScheduled = true;
      entry.ackTimer = setTimeout(() => {
        entry.ackTimer = undefined;
        void sendAck(entry, retry + 1);
      }, delay);
    } else {
      retryExhausted = true;
      if (!entry.ackWarningShown) {
        entry.ackWarningShown = true;
        entry.terminal.writeln(`\r\n[终端输出确认失败，已暂停自动重试：${String(error)}。切换会话后返回可重新连接。]\r\n`);
      }
    }
  } finally {
    entry.ackInFlight = false;
    // 若确认期间又消费了新输出，成功后补发新游标；失败则沿用上面的有限重试。
    if (!entry.disposed && !retryScheduled && !retryExhausted && entry.acknowledged > entry.ackConfirmed) scheduleAck(entry, 0);
  }
}

function acknowledge(entry: TerminalEntry, seq: number) {
  entry.acknowledged = Math.max(entry.acknowledged, seq);
  // ACK 表示 xterm 解析器已经消费字节；失败时只做有限重试，避免 64 条在途事件永久卡住。
  scheduleAck(entry);
}

function discardQueuedInput(entry: TerminalEntry, message?: string) {
  const droppedBytes = entry.inputQueue.reduce((total, item) => total + item.bytes, 0);
  entry.inputQueue.length = 0;
  entry.inputQueuedBytes = Math.max(0, entry.inputQueuedBytes - droppedBytes);
  if (droppedBytes > 0 && message && !entry.disposed) entry.terminal.writeln(`\r\n[${message}]\r\n`);
}

async function drainInput(entry: TerminalEntry) {
  if (entry.inputSending || entry.disposed) return;
  entry.inputSending = true;
  try {
    while (entry.inputQueue.length && !entry.disposed) {
      if (entry.terminal.options.disableStdin) {
        discardQueuedInput(entry, '会话已结束，尚未发送的输入已丢弃');
        break;
      }
      const item = entry.inputQueue.shift()!;
      try {
        await invoke<void>('write_terminal', { sessionId: entry.session.id, data: item.data });
      } catch (error) {
        if (!entry.disposed) entry.terminal.writeln(`\r\n[输入失败：${String(error)}]`);
      } finally {
        entry.inputQueuedBytes = Math.max(0, entry.inputQueuedBytes - item.bytes);
        if (entry.inputQueuedBytes <= MAX_QUEUED_INPUT_BYTES / 2) entry.inputOverflowWarned = false;
      }
    }
  } finally {
    entry.inputSending = false;
  }
}

function enqueueInput(entry: TerminalEntry, data: string) {
  if (!data || entry.disposed || entry.terminal.options.disableStdin) return;
  const bytes = inputEncoder.encode(data).byteLength;
  if (bytes + entry.inputQueuedBytes > MAX_QUEUED_INPUT_BYTES) {
    if (!entry.inputOverflowWarned) {
      entry.inputOverflowWarned = true;
      entry.terminal.writeln('\r\n\x1b[33m[输入未发送：等待队列已超过 1 MiB，请分段粘贴或稍后重试。]\x1b[0m\r\n');
    }
    return;
  }
  entry.inputQueue.push({ data, bytes });
  entry.inputQueuedBytes += bytes;
  void drainInput(entry);
}

function subscribeTerminal(entry: TerminalEntry) {
  if (!isDesktop || entry.disposed || entry.subscription === 'connecting') return;
  const generation = ++entry.generation;
  const channel = new Channel<TerminalEvent>();
  const afterSeq = entry.acknowledged;
  entry.channel = channel;
  entry.subscription = 'connecting';

  // 每次订阅都使用新 Channel，并由 generation 丢弃旧通道迟到的包。
  channel.onmessage = event => {
    if (entry.disposed || generation !== entry.generation) return;
    if (event.sessionId !== entry.session.id || event.serviceId !== entry.session.serviceId || event.runId !== entry.session.runId) return;
    if (event.seq <= entry.acknowledged) { acknowledge(entry, entry.acknowledged); return; }
    // 重订阅会重放尚未 ACK 的事件；仍在 xterm 解析队列中的序号只保留第一次，避免重复输出。
    if (entry.pendingSeqs.has(event.seq)) return;
    entry.pendingSeqs.add(event.seq);
    const consumed = () => {
      entry.pendingSeqs.delete(event.seq);
      acknowledge(entry, event.seq);
    };

    if (event.kind === 'gap') {
      entry.terminal.reset();
      entry.terminal.write(`\r\n\x1b[33m[输出过快，已省略 ${event.droppedBytes.toLocaleString()} 字节历史输出]\x1b[0m\r\n`, consumed);
    } else if (event.kind === 'exit') {
      entry.terminal.options.disableStdin = true;
      discardQueuedInput(entry, '会话已结束，尚未发送的输入已丢弃');
      entry.terminal.write(`\r\n\x1b[90m[会话已结束，退出码 ${event.exitCode ?? '未知'}]\x1b[0m\r\n`, consumed);
    } else {
      // 传递原始 UTF-8 字节，交由 xterm 跨块解码，避免中文被拆包后出现乱码。
      entry.terminal.write(new Uint8Array(event.data), consumed);
    }
  };

  void invoke<void>('subscribe_terminal', { sessionId: entry.session.id, channel, afterSeq }).then(() => {
    if (entry.disposed || generation !== entry.generation) return;
    entry.subscription = 'connected';
    // 后端订阅游标已恢复到 afterSeq；若连接过程中 xterm 又消费了输出，再 ACK 新游标。
    entry.ackConfirmed = Math.max(entry.ackConfirmed, afterSeq);
    entry.ackWarningShown = false;
    if (entry.acknowledged > entry.ackConfirmed) scheduleAck(entry, 0);
  }).catch(error => {
    if (entry.disposed || generation !== entry.generation) return;
    entry.subscription = 'failed';
    entry.terminal.writeln(`[连接终端失败：${String(error)}。切换到其他会话后返回，或再次点选当前标签重试。]`);
  });
}

function applyTerminalAppearance(entry: TerminalEntry, appearance: TerminalAppearance, systemTheme: 'dark' | 'light') {
  const resolved = resolveTerminalAppearance(appearance, systemTheme);
  entry.terminal.options.theme = resolved.colors;
  entry.terminal.options.fontFamily = resolved.fontFamily;
  entry.terminal.options.fontSize = resolved.fontSize;
  entry.terminal.options.lineHeight = resolved.lineHeight;
  entry.terminal.options.cursorStyle = resolved.cursorStyle;
  entry.terminal.options.cursorBlink = resolved.cursorBlink;
  // xterm 自己绘制的背景之外，host 也要同步，避免系统主题的全局 CSS 覆盖它。
  entry.host.style.backgroundColor = resolved.colors.background;
}

export function getTerminal(session: TerminalSession, appearance: TerminalAppearance, systemTheme: 'dark' | 'light'): TerminalEntry {
  const previous = entries.get(session.id);
  if (previous) {
    previous.session = session;
    applyTerminalAppearance(previous, appearance, systemTheme);
    if (session.state === 'exited') previous.terminal.options.disableStdin = true;
    return previous;
  }
  const resolved = resolveTerminalAppearance(appearance, systemTheme);
  const terminal = new Terminal({
    fontFamily: resolved.fontFamily, fontSize: resolved.fontSize, lineHeight: resolved.lineHeight,
    cursorStyle: resolved.cursorStyle, cursorBlink: resolved.cursorBlink, scrollback: 5000, theme: resolved.colors, allowProposedApi: false,
    convertEol: false, screenReaderMode: true, disableStdin: session.state === 'exited',
  });
  const fit = new FitAddon(); terminal.loadAddon(fit);
  const host = document.createElement('div'); host.className = 'terminal-emulator'; host.style.backgroundColor = resolved.colors.background;
  // 实例不归 React 页签组件所有。卸载页签时只移走 DOM，进程、解析器及回滚区继续存在。
  terminal.open(host);
  const entry: TerminalEntry = {
    terminal, fit, host, generation: 0, subscription: 'idle', acknowledged: 0, ackConfirmed: 0,
    pendingSeqs: new Set(), ackInFlight: false, ackWarningShown: false, disposed: false,
    inputQueue: [], inputQueuedBytes: 0, inputSending: false, inputOverflowWarned: false, session,
  };
  entries.set(session.id, entry);
  terminal.onData(data => enqueueInput(entry, data));
  terminal.attachCustomKeyEventHandler(event => {
    if (event.type !== 'keydown') return true;
    const key = event.key.toLowerCase();
    const copy = (event.metaKey || (event.ctrlKey && event.shiftKey)) && key === 'c';
    if (copy && terminal.hasSelection()) { void navigator.clipboard.writeText(terminal.getSelection()); return false; }
    const paste = (event.metaKey || (event.ctrlKey && event.shiftKey)) && key === 'v';
    if (paste) { void navigator.clipboard.readText().then(value => terminal.paste(value)).catch(() => {}); return false; }
    return true;
  });
  if (isDesktop) subscribeTerminal(entry);
  return entry;
}

/** 激活已有页签时仅换 Channel，不销毁 xterm 或其滚动缓冲。 */
export function activateTerminal(entry: TerminalEntry) {
  if (!isDesktop || entry.disposed || entry.subscription === 'connecting') return;
  // 已连接的旧 entry 也需要在切回时换新 Channel；失败或初次连接则在此重试。
  subscribeTerminal(entry);
}

export function resizeTerminal(entry: TerminalEntry) {
  if (entry.disposed || !entry.host.isConnected || entry.host.clientWidth < 20 || entry.host.clientHeight < 20) return;
  // xterm 自己在重排行列后同步滚动位置，并保留用户的历史阅读状态。
  // 不在 fit 后立即 scrollToBottom：此时它的滚动容器可能还在等待下一帧同步，
  // 提前用旧的像素位置滚动反而会把已在底部的缓冲区拉回历史。
  entry.fit.fit();
  if (isDesktop && entry.session.state === 'running') {
    void invoke('resize_terminal', { sessionId: entry.session.id, cols: entry.terminal.cols, rows: entry.terminal.rows }).catch(() => {});
  }
}
/** 只更新已存在的 xterm 样式；不重建实例、不重订阅，因此会话与回滚区保持不变。 */
export function updateTerminalAppearance(appearance: TerminalAppearance, systemTheme: 'dark' | 'light') {
  entries.forEach(entry => {
    applyTerminalAppearance(entry, appearance, systemTheme);
    // 已挂载实例立即重新测量字体尺寸；未挂载实例在 TerminalPanel 切回时再 fit。
    if (entry.host.isConnected) resizeTerminal(entry);
  });
}
function disposeMissing(sessions: TerminalSession[]) {
  const ids = new Set(sessions.map(session => session.id));
  entries.forEach((entry, id) => {
    if (ids.has(id)) return;
    entry.disposed = true;
    entry.generation++;
    clearTimeout(entry.ackTimer);
    if (entry.channel) entry.channel.onmessage = () => {};
    discardQueuedInput(entry);
    entry.pendingSeqs.clear();
    entry.terminal.dispose();
    entry.host.remove();
    entries.delete(id);
  });
}
if (isDesktop) {
  void listen<TerminalSession[]>('terminal-sessions', event => disposeMissing(event.payload));
}
