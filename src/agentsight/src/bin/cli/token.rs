//! Token query subcommand

use agentsight::{
    SqliteConfig, TimePeriod, TokenQueryResult, TokenStore, Trend, format_tokens_with_commas,
};
use structopt::StructOpt;

/// Token query subcommand
#[derive(Debug, StructOpt, Clone)]
pub struct TokenCommand {
    /// Query by fixed time period
    #[structopt(long, possible_values = &["today", "yesterday", "week", "last_week", "month", "last_month"])]
    pub period: Option<String>,

    /// Query last N hours
    #[structopt(long)]
    pub hours: Option<u64>,

    /// Compare with previous period
    #[structopt(long)]
    pub compare: bool,

    /// Output as JSON
    #[structopt(long)]
    pub json: bool,

    /// Filter by process ID
    #[structopt(long)]
    pub pid: Option<u32>,

    /// Include descendant processes (walk ppid tree)
    #[structopt(long, requires = "pid")]
    pub descendants: bool,

    /// Custom data file path
    #[structopt(long)]
    pub data_file: Option<String>,
}

impl TokenCommand {
    pub fn execute(&self) {
        // Determine data file path
        // Use the unified database path (agentsight.db) as default,
        // which is where Storage writes all tables.
        let data_path = self
            .data_file
            .as_ref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| SqliteConfig::default().db_path());

        if let Some(pid) = self.pid {
            self.execute_pid_summary(&data_path, pid);
        } else {
            self.execute_summary(&data_path);
        }
    }

    fn execute_pid_summary(&self, data_path: &std::path::Path, pid: u32) {
        if self.compare {
            eprintln!("Note: --compare is not supported with --pid and is ignored.");
        }

        let (start_ns, end_ns, label) = if let Some(hours) = self.hours {
            (
                super::hours_ago_ns(hours),
                super::hours_ago_ns(0),
                format!("最近 {hours} 小时"),
            )
        } else {
            let period = self
                .period
                .as_deref()
                .map(super::parse_period)
                .unwrap_or(TimePeriod::Today);
            let (start_ns, end_ns) = period.time_range();
            (start_ns, end_ns, period.to_string())
        };

        let pids = if self.descendants {
            agentsight::utils::proc_tree::expand_with_descendants(&[pid])
        } else {
            vec![pid]
        };
        let scope = if self.descendants {
            format!("{label}（PID {pid} 及后代进程）")
        } else {
            format!("{label}（PID {pid}）")
        };

        let store = TokenStore::new(data_path);
        let query = agentsight::TokenQuery::new(&store);
        let result = query.by_pids(&pids, start_ns, end_ns, scope);

        if self.json {
            println!("{}", serde_json::to_string_pretty(&result).unwrap());
        } else {
            print_human_readable(&result, false);
        }
    }

    fn execute_summary(&self, data_path: &std::path::Path) {
        // Open token store
        let store = TokenStore::new(data_path);
        let query = agentsight::TokenQuery::new(&store);

        // Execute query
        let result = if let Some(hours) = self.hours {
            if self.compare {
                query.by_hours_with_compare(hours)
            } else {
                query.by_hours(hours)
            }
        } else if let Some(ref period_str) = self.period {
            let period = super::parse_period(period_str);
            if self.compare {
                query.by_period_with_compare(period)
            } else {
                query.by_period(period)
            }
        } else if self.compare {
            query.by_period_with_compare(TimePeriod::Today)
        } else {
            query.by_period(TimePeriod::Today)
        };

        // Output result
        if self.json {
            println!("{}", serde_json::to_string_pretty(&result).unwrap());
        } else {
            print_human_readable(&result, self.compare);
        }
    }
}

/// Print human-readable summary output
fn print_human_readable(result: &TokenQueryResult, show_compare: bool) {
    // Main result
    println!(
        "{}共消耗 {} tokens。",
        result.period,
        format_tokens_with_commas(result.total_tokens)
    );

    // Comparison
    #[allow(clippy::collapsible_if)]
    if show_compare {
        if let Some(ref comp) = result.comparison {
            let trend = match comp.trend {
                Trend::Up => "增长",
                Trend::Down => "下降",
                Trend::Flat => "持平",
            };

            println!(
                "比上一时段（{}）{}了 {}。",
                format_tokens_with_commas(comp.previous_total),
                trend,
                comp.formatted_change()
            );
        }
    }

    // Additional details
    if result.request_count > 0 {
        println!();
        println!(
            "共 {} 次请求，输入 {} tokens，输出 {} tokens。",
            result.request_count,
            format_tokens_with_commas(result.input_tokens),
            format_tokens_with_commas(result.output_tokens)
        );
    }
}
