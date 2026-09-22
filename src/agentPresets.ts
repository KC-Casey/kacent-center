import catalog from '../assets/agent-presets.json';
import type { AgentProfile } from './types';

export interface AgentPreset extends AgentProfile { description: string; website: string }
// 前端模板与 Rust 数据迁移共同读取此目录，避免默认命令、名称和参数分别维护后发生偏差。
export const agentPresets: AgentPreset[] = catalog;
export const expandedPresetIds = new Set(['opencode', 'hermes', 'zcode', 'kimi', 'aider', 'copilot']);

function executableName(value: string) {
  return value.trim().replace(/^["']+|["']+$/g, '').replaceAll('\\', '/').split('/').at(-1)?.replace(/\.(exe|cmd|bat)$/i, '').toLowerCase() ?? '';
}

export function matchesAgentPreset(agent: AgentProfile, preset: AgentPreset) {
  return agent.id.trim().toLowerCase() === preset.id.toLowerCase() || agent.name.trim().toLowerCase() === preset.name.toLowerCase()
    || executableName(agent.executable) === executableName(preset.executable);
}

export function presetProfile(preset: AgentPreset): AgentProfile {
  return { id: preset.id, name: preset.name, executable: preset.executable, args: [...preset.args], env: { ...preset.env } };
}
