//! 九种内置 CLI 的只读能力检查：不请求模型、不登录、不连接 MCP，也不读取正式数据库。
//! 输出只含工具 ID、是否找到程序、受限计划入口及原因，不输出配置中的环境变量。
use kacent_center_lib::{content_agent, models::AgentProfile};
fn main() -> Result<(), String> {
    let profiles: Vec<AgentProfile> =
        serde_json::from_str(include_str!("../../assets/agent-presets.json"))
            .map_err(|error| error.to_string())?;
    for capability in content_agent::capabilities(&profiles) {
        println!(
            "{}：终端={}，中心计划={}，模式={}，原因={}",
            capability.name,
            if capability.available {
                "已找到"
            } else {
                "未找到"
            },
            if capability.can_plan {
                "限制参数可用，模型调用未验证"
            } else {
                "不可用"
            },
            capability.mode,
            capability
                .reason
                .unwrap_or_else(|| capability.limitations.join("；"))
        );
    }
    Ok(())
}
