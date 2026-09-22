import type { DiscoveredProcess, PortInfo, ProcessIdentity } from './types';

export type DiscoveryProcessCategory = DiscoveredProcess['instance']['category'];
export type DiscoveryGroupCategory = DiscoveryProcessCategory | 'mixed';

/** 同一协议与端口的汇总项，地址和所属进程由成员记录汇总而来。 */
export interface DiscoveryGroupPort {
  protocol: PortInfo['protocol'];
  port: number;
  addresses: string[];
  ownerPids: number[];
}

/** 发现页展示的一组相关进程；操作仍引用其中的原始进程记录。 */
export interface DiscoveryProcessGroup {
  key: string;
  rootIdentity: ProcessIdentity;
  members: DiscoveredProcess[];
  primary: DiscoveredProcess;
  category: DiscoveryGroupCategory;
  ports: DiscoveryGroupPort[];
}

/** 进程创建标识同时包含 PID 与创建时间，避免 PID 重用时误认成同一进程。 */
export function processIdentityKey(identity: ProcessIdentity): string {
  return `${identity.pid}:${identity.createdAt}`;
}

/**
 * 获取后端已核验的服务组根标识；没有根标识时仅以本进程自身为组。
 * 不按名称、可执行文件或端口合并，避免把互不相关的同名服务误当成一组。
 */
export function discoveryGroupIdentity(process: DiscoveredProcess): ProcessIdentity {
  return process.familyIdentity ?? process.instance.identity;
}

function choosePrimary(members: DiscoveredProcess[], rootIdentity: ProcessIdentity): DiscoveredProcess {
  const rootKey = processIdentityKey(rootIdentity);
  const rootMember = members.find((member) => processIdentityKey(member.instance.identity) === rootKey);
  if (rootMember) return rootMember;

  // 根进程不在当前扫描结果中时，按 PID 与创建时间排序，保证代表进程稳定。
  return [...members].sort((a, b) => a.instance.identity.pid - b.instance.identity.pid
    || a.instance.identity.createdAt.localeCompare(b.instance.identity.createdAt))[0];
}

function summarizePorts(members: DiscoveredProcess[]): DiscoveryGroupPort[] {
  const portsByKey = new Map<string, { protocol: PortInfo['protocol']; port: number; addresses: Set<string>; ownerPids: Set<number> }>();

  for (const member of members) {
    for (const port of member.ports) {
      // 多个同组进程可能重复报告共享 UDP/TCP 端口；每种协议与端口只保留一项。
      const key = `${port.protocol}:${port.port}`;
      const summary = portsByKey.get(key) ?? {
        protocol: port.protocol,
        port: port.port,
        addresses: new Set<string>(),
        ownerPids: new Set<number>(),
      };
      summary.addresses.add(port.address);
      // 用成员身份表达该端口关联的进程，避免重复报告让地址或所属 PID 丢失。
      summary.ownerPids.add(member.instance.identity.pid);
      portsByKey.set(key, summary);
    }
  }

  return [...portsByKey.values()]
    .map((port) => ({
      protocol: port.protocol,
      port: port.port,
      addresses: [...port.addresses].sort((a, b) => a.localeCompare(b)),
      ownerPids: [...port.ownerPids].sort((a, b) => a - b),
    }))
    .sort((a, b) => (a.protocol === b.protocol ? 0 : a.protocol === 'TCP' ? -1 : 1) || a.port - b.port);
}

/**
 * 依照后端提供的进程族根 identity 聚合结果。函数只构造展示数据，不会伪造
 * DiscoveredProcess；接入、查看详情等回调仍可安全地使用具体成员原始记录。
 */
export function groupDiscoveredProcesses(processes: DiscoveredProcess[]): DiscoveryProcessGroup[] {
  const membersByRoot = new Map<string, { rootIdentity: ProcessIdentity; members: DiscoveredProcess[] }>();

  for (const process of processes) {
    const rootIdentity = discoveryGroupIdentity(process);
    const key = processIdentityKey(rootIdentity);
    const group = membersByRoot.get(key) ?? { rootIdentity, members: [] };
    group.members.push(process);
    membersByRoot.set(key, group);
  }

  return [...membersByRoot.entries()]
    .map(([key, value]) => {
      const members = [...value.members].sort((a, b) => a.instance.identity.pid - b.instance.identity.pid
        || a.instance.identity.createdAt.localeCompare(b.instance.identity.createdAt));
      const categories = new Set(members.map((member) => member.instance.category));
      const category: DiscoveryGroupCategory = categories.size > 1 ? 'mixed' : members[0]?.instance.category ?? 'unknown';
      return {
        key,
        rootIdentity: value.rootIdentity,
        members,
        primary: choosePrimary(members, value.rootIdentity),
        category,
        ports: summarizePorts(members),
      };
    })
    .sort((a, b) => (a.primary.instance.name || '').localeCompare(b.primary.instance.name || '')
      || a.primary.instance.identity.pid - b.primary.instance.identity.pid
      || a.primary.instance.identity.createdAt.localeCompare(b.primary.instance.identity.createdAt));
}
