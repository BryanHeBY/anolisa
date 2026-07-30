//! Discover subcommand - scan for running AI agents
//!
//! This module provides the `discover` subcommand which scans the system
//! for running AI agent processes.

use agentsight::{AgentScanner, CmdlineGlobMatcher, ProcessContext};
use structopt::StructOpt;

/// Discover subcommand for finding AI agents running on the system
#[derive(Debug, StructOpt, Clone)]
pub struct DiscoverCommand {
    /// Show detailed output including executable path
    #[structopt(short, long)]
    pub verbose: bool,

    /// List all known agents and show currently matched PIDs
    #[structopt(long)]
    pub list_known: bool,

    /// Output as JSON
    #[structopt(long)]
    pub json: bool,
}

impl DiscoverCommand {
    pub fn execute(&self) {
        if self.list_known {
            self.list_known_agents();
            return;
        }

        self.scan_agents();
    }

    /// List all known agents that can be detected
    fn list_known_agents(&self) {
        let rules = agentsight::default_cmdline_rules();
        let matchers: Vec<CmdlineGlobMatcher> = rules
            .iter()
            .filter_map(CmdlineGlobMatcher::from_config)
            .collect();
        let mut scanner = AgentScanner::from_rules(&rules, &[]);
        let running_agents = scanner.scan();

        if self.json {
            let items: Vec<serde_json::Value> = matchers
                .iter()
                .map(|matcher| {
                    let agent = matcher.info();
                    serde_json::json!({
                        "name": agent.name,
                        "category": agent.category,
                        "description": agent.description,
                        "patterns": matcher.patterns(),
                        "running_pids": matched_pids(matcher, &running_agents),
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&items).unwrap_or_else(|_| "[]".to_string())
            );
            return;
        }

        println!("已知 AI Agent（共 {} 条规则）:", matchers.len());
        println!("{}", "=".repeat(60));
        println!();

        for matcher in &matchers {
            let agent = matcher.info();
            let matched: Vec<String> = matched_pids(matcher, &running_agents)
                .iter()
                .map(u32::to_string)
                .collect();
            let running_pids = if matched.is_empty() {
                "无".to_string()
            } else {
                matched.join(", ")
            };

            println!("  {} ({})", agent.name, agent.category);
            println!("    命令行规则: {}", matcher.patterns().join(" "));
            println!("    运行中 PID: {running_pids}");
            println!("    {}", agent.description);
            println!();
        }
    }

    /// Scan the system for running AI agents
    fn scan_agents(&self) {
        let mut scanner = AgentScanner::from_rules(&agentsight::default_cmdline_rules(), &[]);
        let agents = scanner.scan();

        if self.json {
            let items: Vec<serde_json::Value> = agents
                .iter()
                .map(|agent| {
                    serde_json::json!({
                        "pid": agent.pid,
                        "name": agent.agent_info.name,
                        "category": agent.agent_info.category,
                        "description": agent.agent_info.description,
                        "cmdline": agent.cmdline_args,
                        "exe_path": agent.exe_path,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&items).unwrap_or_else(|_| "[]".to_string())
            );
            return;
        }

        if agents.is_empty() {
            println!("未发现正在运行的 AI Agent。");
            println!();
            println!("提示：使用 --list-known 查看所有可检测的 Agent。");
            return;
        }

        println!("已发现 AI Agent（共 {} 个）:", agents.len());
        println!("{}", "=".repeat(60));
        println!();

        for agent in &agents {
            println!("  {} [PID: {}]", agent.agent_info.name, agent.pid);
            println!("    类别: {}", agent.agent_info.category);

            // Truncate long command lines
            let cmdline_str = agent.cmdline_args.join(" ");
            let cmdline = if cmdline_str.len() > 80 && !self.verbose {
                format!("{}...", &cmdline_str[..77])
            } else {
                cmdline_str
            };
            println!("    命令:  {cmdline}");

            if self.verbose && !agent.exe_path.is_empty() {
                println!("    可执行文件: {}", agent.exe_path);
            }

            println!();
        }

        println!("总计: {} 个 Agent", agents.len());
    }
}

/// Return PIDs of running agents whose process context matches `matcher`.
fn matched_pids(
    matcher: &CmdlineGlobMatcher,
    running_agents: &[agentsight::DiscoveredAgent],
) -> Vec<u32> {
    running_agents
        .iter()
        .filter(|agent| {
            let ctx = ProcessContext {
                comm: String::new(),
                cmdline_args: agent.cmdline_args.clone(),
                exe_path: agent.exe_path.clone(),
            };
            matcher.matches(&ctx)
        })
        .map(|agent| agent.pid)
        .collect()
}
