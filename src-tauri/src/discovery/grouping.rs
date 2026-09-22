//! 仅供发现快照展示使用的保守进程族推断。
//!
//! 这里不决定停止范围，也不把名称、PID、端口相同当作进程关系。调用端必须先确认
//! 每个候选都是本轮 socket owner，并在前后核对完整原生创建身份。Unix 可从 native
//! 查询读取父 PID；Windows 从同一轮 sysinfo 样本读取父 PID，同时复核父子双方身份。本
//! 模块只连接输入中可证明的直接父子关系，并把最上游 socket owner 身份作为展示提示。

use crate::models::ProcessIdentity;
use std::collections::{HashMap, HashSet};

/// 一个已由发现器校验过、且在同一快照中持有 socket 的进程。
#[derive(Clone, Debug)]
pub(super) struct ProcessFamilyCandidate {
    /// 包含 PID 和精确创建时间，不能只凭 PID 判断候选是否相同。
    pub identity: ProcessIdentity,
    /// Unix 从 native query 读取、Windows 从同轮 sysinfo 样本读取的直接父 PID。
    /// 调用端已核验父子双方的原生创建身份，并在支持的平台复核 native 父 PID 不变。
    pub parent_pid: Option<u32>,
    /// 必须由调用端规范化后的 executable 路径；路径无法规范化时留空并禁止合并。
    pub executable_key: Option<String>,
    /// 进程分类必须相同，避免把系统进程和用户进程放入同一展示族。
    pub category: String,
    /// Windows 共享或潜在共享宿主永远单独显示。
    pub shared_host: bool,
    /// 是否为通用解释器；解释器必须同时提供完整的 cwd 与脚本来源键。
    pub is_interpreter: bool,
    /// 规范化后的 cwd 与脚本来源；未知来源解释器留空并禁止合并。
    pub interpreter: Option<InterpreterFamilySource>,
}

/// 解释器的脚本来源键。cwd 与脚本路径都应是调用端规范化后的绝对路径。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct InterpreterFamilySource {
    pub working_dir_key: String,
    pub source_key: String,
}

/// 返回每个多成员进程族的展示身份；单进程和无法证明关系的进程不出现在结果中。
///
/// 连接边只可能来自“同一快照中的 socket owner 子进程 → socket owner 直接父进程”，且
/// 调用端必须先核验两端完整身份。Windows 使用 sysinfo 的父 PID 时，创建时间先后关系还
/// 用于排除父 PID 已复用成在子进程创建之后才启动的无关进程。
/// 因此某个中间 supervisor 即使不持有端口，也不会把彼此独立的服务跨层合并。异常的
/// 重复 PID、缺失创建时间、共享宿主、身份键缺失和循环父链都采取不合并的保守处理。
pub(super) fn family_identity_by_pid(
    candidates: &[ProcessFamilyCandidate],
) -> HashMap<u32, ProcessIdentity> {
    let by_pid = unique_valid_candidates(candidates);
    let mut parent_by_child = HashMap::<u32, u32>::new();

    for (&child_pid, child) in &by_pid {
        let Some(parent_pid) = child.parent_pid else {
            continue;
        };
        let Some(parent) = by_pid.get(&parent_pid) else {
            // 只沿当前 socket owner 集合中的直接父子关系合并；不追踪非 owner supervisor。
            continue;
        };
        if can_link(parent, child) {
            parent_by_child.insert(child_pid, parent_pid);
        }
    }

    // 沿已确认的父链找到最上游 owner。发现循环时不对该链上的任何成员输出族身份，
    // 避免异常或伪造的父 PID 关系造成跨进程误合并。
    let mut roots = HashMap::<u32, u32>::new();
    for &pid in by_pid.keys() {
        if let Some(root_pid) = find_root(pid, &parent_by_child) {
            roots.insert(pid, root_pid);
        }
    }

    let mut family_sizes = HashMap::<u32, usize>::new();
    for root_pid in roots.values() {
        *family_sizes.entry(*root_pid).or_default() += 1;
    }

    roots
        .into_iter()
        .filter_map(|(pid, root_pid)| {
            if family_sizes.get(&root_pid).copied().unwrap_or_default() <= 1 {
                return None;
            }
            Some((pid, by_pid.get(&root_pid)?.identity.clone()))
        })
        .collect()
}

/// 去除身份不完整和重复 PID 候选；有歧义的 PID 不能作为父或子参与关系证明。
fn unique_valid_candidates<'a>(
    candidates: &'a [ProcessFamilyCandidate],
) -> HashMap<u32, &'a ProcessFamilyCandidate> {
    let mut by_pid = HashMap::new();
    let mut duplicate_pids = HashSet::new();
    for candidate in candidates {
        if candidate.identity.pid == 0 || candidate.identity.created_at.trim().is_empty() {
            continue;
        }
        let pid = candidate.identity.pid;
        if by_pid.insert(pid, candidate).is_some() {
            duplicate_pids.insert(pid);
        }
    }
    for pid in duplicate_pids {
        by_pid.remove(&pid);
    }
    by_pid
}

/// 只在可执行文件、分类与安全条件全部一致时保留直接父子边。
fn can_link(parent: &ProcessFamilyCandidate, child: &ProcessFamilyCandidate) -> bool {
    if parent.shared_host
        || child.shared_host
        || parent.category != child.category
        || parent.executable_key.is_none()
        || parent.executable_key != child.executable_key
        || !parent_created_before_child(parent, child)
    {
        return false;
    }

    match (
        parent.is_interpreter,
        child.is_interpreter,
        &parent.interpreter,
        &child.interpreter,
    ) {
        // 普通可执行文件不需要脚本来源；解释器必须是相同 cwd 下运行相同脚本。
        (false, false, None, None) => true,
        (true, true, Some(parent_source), Some(child_source)) => parent_source == child_source,
        // 不让解释器与普通程序混组，也不把来源不明的解释器推断成一个服务。
        _ => false,
    }
}

/// 原生创建时间在各平台均以十进制字符串保存；父进程不得晚于子进程创建。
fn parent_created_before_child(
    parent: &ProcessFamilyCandidate,
    child: &ProcessFamilyCandidate,
) -> bool {
    let (Ok(parent_created_at), Ok(child_created_at)) = (
        parent.identity.created_at.parse::<u128>(),
        child.identity.created_at.parse::<u128>(),
    ) else {
        return false;
    };
    parent_created_at <= child_created_at
}

/// 找到直接父链的根；父关系循环时返回 None。
fn find_root(pid: u32, parent_by_child: &HashMap<u32, u32>) -> Option<u32> {
    let mut current = pid;
    let mut visited = HashSet::new();
    visited.insert(current);
    while let Some(parent_pid) = parent_by_child.get(&current).copied() {
        if !visited.insert(parent_pid) {
            return None;
        }
        current = parent_pid;
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::{family_identity_by_pid, InterpreterFamilySource, ProcessFamilyCandidate};
    use crate::models::ProcessIdentity;

    fn candidate(
        pid: u32,
        created_at: &str,
        parent_pid: Option<u32>,
        executable_key: &str,
    ) -> ProcessFamilyCandidate {
        ProcessFamilyCandidate {
            identity: ProcessIdentity {
                pid,
                created_at: created_at.into(),
            },
            parent_pid,
            executable_key: Some(executable_key.into()),
            category: "nonSystem".into(),
            shared_host: false,
            is_interpreter: false,
            interpreter: None,
        }
    }

    #[test]
    fn one_socket_parent_and_multiple_child_owners_share_root_identity() {
        let parent = candidate(2801, "100", None, "/usr/local/bin/postgres");
        let first_child = candidate(2802, "200", Some(2801), "/usr/local/bin/postgres");
        let second_child = candidate(2803, "300", Some(2801), "/usr/local/bin/postgres");

        let families = family_identity_by_pid(&[parent.clone(), first_child, second_child]);

        assert_eq!(families.get(&2801), Some(&parent.identity));
        assert_eq!(families.get(&2802), Some(&parent.identity));
        assert_eq!(families.get(&2803), Some(&parent.identity));
    }

    #[test]
    fn unrelated_same_executable_processes_remain_separate() {
        let left = candidate(2801, "100", None, "/usr/local/bin/postgres");
        let right = candidate(3100, "200", None, "/usr/local/bin/postgres");

        assert!(family_identity_by_pid(&[left, right]).is_empty());
    }

    #[test]
    fn parent_and_child_with_different_categories_remain_separate() {
        let mut parent = candidate(100, "100", None, "/usr/local/bin/postgres");
        parent.category = "system".into();
        let child = candidate(101, "200", Some(100), "/usr/local/bin/postgres");

        assert!(family_identity_by_pid(&[parent, child]).is_empty());
    }

    #[test]
    fn different_interpreter_scripts_and_shared_hosts_do_not_merge() {
        let mut parent = candidate(100, "100", None, "/usr/bin/node");
        parent.is_interpreter = true;
        parent.interpreter = Some(InterpreterFamilySource {
            working_dir_key: "/srv/app".into(),
            source_key: "/srv/app/server-a.js".into(),
        });
        let mut child = candidate(101, "101", Some(100), "/usr/bin/node");
        child.is_interpreter = true;
        child.interpreter = Some(InterpreterFamilySource {
            working_dir_key: "/srv/app".into(),
            source_key: "/srv/app/server-b.js".into(),
        });
        assert!(family_identity_by_pid(&[parent.clone(), child]).is_empty());

        let mut shared_child = candidate(102, "102", Some(100), "/usr/bin/node");
        shared_child.is_interpreter = true;
        shared_child.interpreter = parent.interpreter.clone();
        shared_child.shared_host = true;
        assert!(family_identity_by_pid(&[parent, shared_child]).is_empty());
    }

    #[test]
    fn a_parent_created_after_its_child_is_not_a_proven_ancestor() {
        let parent = candidate(100, "200", None, "/usr/local/bin/postgres");
        let child = candidate(101, "100", Some(100), "/usr/local/bin/postgres");

        assert!(family_identity_by_pid(&[parent, child]).is_empty());
    }
}
