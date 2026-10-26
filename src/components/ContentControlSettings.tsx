import { useEffect, useState } from 'react';
import { Copy, Download, Link, Mic, RefreshCw, Upload } from 'lucide-react';
import { open } from '@tauri-apps/plugin-dialog';
import { isDesktop } from '../api';
import { contentApi } from '../contentApi';
import type { AgentCapabilities, ControlPreferences, SpeechStatus } from '../contentTypes';
import Select from './Select';
import './content.css';

/** 接入片段只包含程序路径；认证凭据保留在本机受限文件中，从不复制到 Agent 配置。 */
export default function ContentControlSettings() {
  const [capabilities, setCapabilities] = useState<AgentCapabilities[]>([]);
  const [preferences, setPreferences] = useState<ControlPreferences>({ agentId: '', voiceEnabled: false });
  const [speech, setSpeech] = useState<SpeechStatus | null>(null);
  const [selected, setSelected] = useState('codex'); const [config, setConfig] = useState(''); const [configNote, setConfigNote] = useState('');
  const [error, setError] = useState(''); const [message, setMessage] = useState(''); const [busy, setBusy] = useState(false);
  const refresh = async () => { try { const [caps, prefs, status] = await Promise.all([contentApi.capabilities(), contentApi.preferences(), contentApi.speechStatus()]); setCapabilities(caps); setPreferences(prefs); setSpeech(status); window.dispatchEvent(new Event('content-preferences-changed')); } catch (e) { setError(String(e)); } };
  useEffect(() => { void refresh(); const timer = window.setInterval(() => { void contentApi.speechStatus().then(setSpeech).catch(() => {}); }, 1000); return () => clearInterval(timer); }, []);
  useEffect(() => { if (!isDesktop) return; let active = true; void contentApi.mcpConfig(selected).then(value => { if (active) { const guide = value as { configuration?: unknown; location?: string; notes?: string[] }; setConfig(typeof guide.configuration === 'string' ? guide.configuration : guide.configuration ? JSON.stringify(guide.configuration, null, 2) : ''); setConfigNote([guide.location, ...(guide.notes || [])].filter(Boolean).join('\n')); } }).catch(e => { if (active) setConfig(String(e)); }); return () => { active = false; }; }, [selected]);
  const work = async (callback: () => Promise<unknown>, success = '') => { setBusy(true); setError(''); setMessage(''); try { await callback(); setMessage(success); await refresh(); } catch (e) { setError(String(e)); } finally { setBusy(false); } };
  const change = (next: ControlPreferences) => { void work(async () => { setPreferences(await contentApi.savePreferences(next)); }, '内容控制偏好已保存'); };
  const installing = speech?.state === 'downloading' || speech?.state === 'importing';
  return <section className="content-control-settings">
    <div className="section-heading"><h3>内容库与 Agent</h3><p>文字与语音指令仅管理内容库。以下偏好修改后即时保存。</p></div>
    {!isDesktop && <p className="content-notice">当前为浏览器预览，CLI 检测、MCP 与录音仅在桌面应用可用。</p>}
    <label className="field"><span>默认指令工具</span><Select value={preferences.agentId} disabled={!isDesktop || busy} onChange={agentId => change({ ...preferences, agentId })} options={[{ value: '', label: '每次选择' }, ...capabilities.map(c => ({ value: c.agentId, label: `${c.name}${c.centerAvailable ? '' : '（暂不可用）'}`, disabled: !c.centerAvailable }))]} /></label>
    <div className="content-capabilities"><table><thead><tr><th>工具</th><th>终端</th><th>中心指令</th><th>外部 MCP</th></tr></thead><tbody>{capabilities.map(c => <tr key={c.agentId} title={c.reason || undefined}><td>{c.name}</td><td>{c.terminalAvailable ? '已找到' : '未找到'}</td><td>{c.centerAvailable ? '可用' : c.centerSupported ? '受限' : '未启用'}</td><td>{c.mcpSupported ? '支持接入' : '不支持'}</td></tr>)}</tbody></table></div>
    <p className="field-hint">“已找到”仅代表可执行文件可用，不代表已登录或完成模型调用验收。</p>
    {capabilities.some(c => c.reason) && <details className="content-tool-reasons"><summary>查看工具限制与不可用原因</summary>{capabilities.filter(c => c.reason).map(c => <p key={c.agentId}><strong>{c.name}</strong>：{c.reason}</p>)}</details>}
    <div className="content-inline-actions"><button type="button" className="button secondary compact" disabled={!isDesktop || busy} onClick={() => void work(refresh)}><RefreshCw size={14} />重新检查工具</button></div>
    <h4><Link size={16} />外部 Agent 接入</h4>
    <label className="field"><span>接入配置</span><Select value={selected} onChange={setSelected} options={['codex', 'claude', 'gemini', 'opencode', 'hermes', 'zcode', 'kimi', 'copilot', 'aider'].map(id => ({ value: id, label: id }))} /></label>
    <p className="field-hint" style={{ whiteSpace: 'pre-wrap' }}>{configNote}</p><pre className="content-config">{config || (isDesktop ? '此工具暂无可生成的接入配置。' : '请在桌面应用中生成配置。')}</pre>
    <div className="content-inline-actions"><button type="button" className="button secondary compact" disabled={!config} onClick={() => void work(() => navigator.clipboard.writeText(config), '接入说明已复制')}><Copy size={14} />复制配置</button><button type="button" className="button secondary compact" disabled={!isDesktop || busy} onClick={() => void work(contentApi.checkMcp, '本机认证通道连接成功')}><Link size={14} />检查连接</button></div>
    <p className="field-hint">将对应片段合并到工具配置，保留原有设置。中心必须处于运行状态。</p>
    <h4><Mic size={16} />本地语音</h4>
    <label className="content-check"><input type="checkbox" checked={preferences.voiceEnabled} disabled={!isDesktop || busy} onChange={e => change({ ...preferences, voiceEnabled: e.target.checked })} />启用按住说话</label>
    <p>多语言 base 模型约 142 MiB。录音与转写在本机处理；转写后的文字及候选收藏信息会交给所选 Agent，沿用其模型与网络配置。</p>
    <div className="content-model-state" role="status">{speech?.modelReady ? '已找到本地模型，可离线识别中文' : '尚未准备语音模型'}{installing && <><progress max={100} value={speech?.progress || 0} />{speech?.progress || 0}%</>}</div>
    <div className="content-inline-actions"><button type="button" className="button secondary compact" disabled={!isDesktop || busy || installing} onClick={() => void work(contentApi.speechDownload, '语音模型下载并校验完成')}><Download size={14} />下载 base 模型</button><button type="button" className="button secondary compact" disabled={!isDesktop || busy || installing} onClick={() => void work(async () => { const path = await open({ title: '导入多语言 ggml-base.bin 模型', multiple: false, filters: [{ name: 'Whisper 模型', extensions: ['bin'] }] }); if (typeof path === 'string') await contentApi.speechImport(path); })}><Upload size={14} />导入模型</button>{installing && <button type="button" className="button secondary compact" onClick={() => void contentApi.speechCancel().then(refresh).catch(e => setError(String(e)))}>取消</button>}</div>
    {(error || speech?.error) && <p className="form-error" role="alert">{error || speech?.error}</p>}{message && <p className="content-success" role="status">{message}</p>}
  </section>;
}
