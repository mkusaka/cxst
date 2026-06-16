use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use codex_backend_client::Client as BackendClient;
use codex_backend_client::TokenUsageProfile;
use codex_backend_client::TokenUsageProfileDailyBucket;
use codex_backend_client::TokenUsageProfileStats;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_model_provider::create_model_provider;
use codex_protocol::account::PlanType;
use codex_protocol::account::ProviderAccount;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::models::ActivePermissionProfile;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_DANGER_FULL_ACCESS;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_READ_ONLY;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_WORKSPACE;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::RateLimitWindow;
use codex_utils_cli::CliConfigOverrides;
use codex_utils_sandbox_summary::summarize_permission_profile;
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(name = "cxst")]
#[command(about = "Show Codex account and rate-limit status.")]
#[command(version)]
struct Cli {
    #[clap(flatten)]
    config_overrides: CliConfigOverrides,

    #[arg(long, global = true, help = "Print machine-readable JSON.")]
    json: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Show Codex account and rate-limit status.")]
    Status,
    #[command(about = "Show Codex account token activity.")]
    Usage(UsageArgs),
    #[command(
        about = "Check whether selected rate-limit remaining usage is above a threshold.",
        after_long_help = "Exit codes:\n  0  selected rate limits are above the threshold\n  1  threshold reached, or rate-limit status is unavailable"
    )]
    Check(CheckArgs),
    #[command(
        about = "Wait until selected rate-limit remaining usage reaches a threshold.",
        after_long_help = "Exit codes:\n  0  timeout reached before the threshold was hit\n  1  threshold reached, or rate-limit status is unavailable"
    )]
    Wait(WaitArgs),
}

#[derive(Debug, Args)]
struct CheckArgs {
    #[arg(
        long = "remaining-percent",
        alias = "threshold",
        default_value = "10",
        value_parser = parse_percent_arg,
        help = "Fail when selected rate limits have this remaining percent or less."
    )]
    remaining_percent: f64,

    #[arg(
        long,
        default_value = "both",
        value_enum,
        help = "Rate-limit window to watch."
    )]
    window: WaitWindow,
}

#[derive(Debug, Args)]
struct WaitArgs {
    #[clap(flatten)]
    check: CheckArgs,

    #[arg(
        short = 'i',
        long,
        default_value = "60s",
        value_parser = parse_duration_arg,
        help = "Polling interval. Supports plain seconds, ms, s, m, or h."
    )]
    interval: Duration,

    #[arg(
        long,
        value_parser = parse_duration_arg,
        help = "Stop successfully if the threshold is not reached before this duration."
    )]
    timeout: Option<Duration>,
}

#[derive(Debug, Args)]
struct UsageArgs {
    #[arg(
        default_value = "daily",
        value_enum,
        help = "Token activity view to render."
    )]
    view: UsageView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
enum UsageView {
    #[value(alias = "day")]
    Daily,
    #[value(alias = "week")]
    Weekly,
    Cumulative,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum WaitWindow {
    #[value(name = "5h", alias = "five-hour", alias = "five_hour")]
    FiveHour,
    Weekly,
    Monthly,
    #[value(alias = "all")]
    Both,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusOutput {
    auth: AuthOutput,
    rate_limits: RateLimitsOutput,
    codex: CodexOutput,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthOutput {
    status: AuthStatus,
    requires_openai_auth: bool,
    email: Option<String>,
    plan_type: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum AuthStatus {
    Chatgpt,
    ApiKey,
    AmazonBedrock,
    Unauthenticated,
    NotRequired,
    Unavailable,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitsOutput {
    status: RateLimitStatus,
    reason: Option<String>,
    limits: Vec<RateLimitOutput>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum RateLimitStatus {
    Available,
    Unavailable,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitOutput {
    scope: String,
    plan_type: Option<String>,
    five_hour: Option<RateLimitWindowOutput>,
    weekly: Option<RateLimitWindowOutput>,
    monthly: Option<RateLimitWindowOutput>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UsageOutput {
    status: UsageStatus,
    reason: Option<String>,
    view: UsageView,
    summary: Option<UsageSummaryOutput>,
    daily_usage_buckets: Option<Vec<UsageDailyBucketOutput>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum UsageStatus {
    Available,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct UsageSummaryOutput {
    lifetime_tokens: Option<i64>,
    peak_daily_tokens: Option<i64>,
    longest_running_turn_sec: Option<i64>,
    current_streak_days: Option<i64>,
    longest_streak_days: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct UsageDailyBucketOutput {
    start_date: String,
    tokens: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitWindowOutput {
    remaining_percent: f64,
    used_percent: f64,
    reset_at: Option<String>,
    #[serde(skip)]
    reset_display: Option<String>,
    window_minutes: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CodexOutput {
    codex_home: String,
    directory: String,
    permissions: String,
    agents_md: Vec<String>,
    collaboration_mode: String,
    model: String,
    model_details: Vec<String>,
    model_provider: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct WaitObservedWindow {
    scope: String,
    window: String,
    remaining_percent: f64,
    used_percent: f64,
    reset_at: Option<String>,
    reset_display: Option<String>,
}

#[derive(Debug, PartialEq)]
enum WaitDecision {
    Continue(Vec<WaitObservedWindow>),
    Triggered(Vec<WaitObservedWindow>),
    Unavailable(String),
}

#[derive(Debug, PartialEq)]
struct CommandOutput {
    exit_code: u8,
    stdout: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WaitEventOutput {
    status: String,
    threshold_remaining_percent: f64,
    windows: Vec<WaitObservedWindow>,
    reason: Option<String>,
    next_poll_seconds: Option<u64>,
}

#[tokio::main]
async fn main() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();

    match &cli.command {
        Some(Command::Usage(args)) => run_usage(&cli, args).await,
        Some(Command::Check(args)) => run_check(&cli, args).await,
        Some(Command::Wait(args)) => run_wait(&cli, args).await,
        Some(Command::Status) | None => {
            let output = load_status(&cli).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                print_human(&output);
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn load_config(cli: &Cli) -> anyhow::Result<Config> {
    let cli_overrides = cli
        .config_overrides
        .parse_overrides()
        .map_err(|error| anyhow::anyhow!("failed to parse -c/--config override: {error}"))?;
    ConfigBuilder::default()
        .cli_overrides(cli_overrides)
        .harness_overrides(ConfigOverrides::default())
        .strict_config(false)
        .build()
        .await
        .context("failed to load Codex config")
}

async fn load_status(cli: &Cli) -> anyhow::Result<StatusOutput> {
    let config = load_config(cli).await?;

    let auth_manager =
        AuthManager::shared_from_config(&config, /*enable_codex_api_key_env*/ true).await;
    let provider = create_model_provider(config.model_provider.clone(), Some(auth_manager.clone()));

    let auth = auth_manager.auth().await;
    let auth_output = match provider.account_state() {
        Ok(account_state) => auth_output_from_account_state(
            account_state.account.as_ref(),
            account_state.requires_openai_auth,
        ),
        Err(_) => AuthOutput {
            status: AuthStatus::Unavailable,
            requires_openai_auth: true,
            email: None,
            plan_type: None,
        },
    };

    let rate_limits = load_rate_limits(&config.chatgpt_base_url, auth.as_ref()).await;
    let codex = codex_output_from_config(&config);

    Ok(StatusOutput {
        auth: auth_output,
        rate_limits,
        codex,
    })
}

async fn run_usage(cli: &Cli, args: &UsageArgs) -> anyhow::Result<ExitCode> {
    let output = load_usage(cli, args.view).await?;
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        print_usage_human(&output);
    }
    Ok(ExitCode::SUCCESS)
}

async fn load_usage(cli: &Cli, view: UsageView) -> anyhow::Result<UsageOutput> {
    let config = load_config(cli).await?;
    let auth_manager =
        AuthManager::shared_from_config(&config, /*enable_codex_api_key_env*/ true).await;
    let auth = auth_manager.auth().await;

    Ok(load_token_usage(&config.chatgpt_base_url, auth.as_ref(), view).await)
}

async fn run_check(cli: &Cli, args: &CheckArgs) -> anyhow::Result<ExitCode> {
    let output = load_status(cli).await?;
    let result = check_rate_limits(&output.rate_limits, args, cli.json)?;
    print!("{}", result.stdout);
    Ok(exit_code(result.exit_code))
}

fn check_rate_limits(
    rate_limits: &RateLimitsOutput,
    args: &CheckArgs,
    json: bool,
) -> anyhow::Result<CommandOutput> {
    match evaluate_wait(rate_limits, args.window, args.remaining_percent) {
        WaitDecision::Triggered(windows) => Ok(CommandOutput {
            exit_code: 1,
            stdout: if json {
                format_wait_json(
                    "threshold_reached",
                    args.remaining_percent,
                    windows,
                    None,
                    None,
                )?
            } else {
                format_wait_triggered(args.remaining_percent, &windows)
            },
        }),
        WaitDecision::Unavailable(reason) => Ok(CommandOutput {
            exit_code: 1,
            stdout: if json {
                format_wait_json(
                    "unavailable",
                    args.remaining_percent,
                    Vec::new(),
                    Some(reason),
                    None,
                )?
            } else {
                format!("rate limits unavailable: {reason}\n")
            },
        }),
        WaitDecision::Continue(windows) => Ok(CommandOutput {
            exit_code: 0,
            stdout: if json {
                format_wait_json("ok", args.remaining_percent, windows, None, None)?
            } else {
                format_check_ok(args.remaining_percent, &windows)
            },
        }),
    }
}

fn exit_code(code: u8) -> ExitCode {
    if code == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(code)
    }
}

async fn run_wait(cli: &Cli, args: &WaitArgs) -> anyhow::Result<ExitCode> {
    let started = Instant::now();

    loop {
        let output = load_status(cli).await?;
        match evaluate_wait(
            &output.rate_limits,
            args.check.window,
            args.check.remaining_percent,
        ) {
            WaitDecision::Triggered(windows) => {
                if cli.json {
                    print_wait_json(
                        "threshold_reached",
                        args.check.remaining_percent,
                        windows,
                        None,
                        None,
                    )?;
                } else {
                    print_wait_triggered(args.check.remaining_percent, &windows);
                }
                return Ok(ExitCode::from(1));
            }
            WaitDecision::Unavailable(reason) => {
                if cli.json {
                    print_wait_json(
                        "unavailable",
                        args.check.remaining_percent,
                        Vec::new(),
                        Some(reason),
                        None,
                    )?;
                } else {
                    println!("rate limits unavailable: {reason}");
                }
                return Ok(ExitCode::from(1));
            }
            WaitDecision::Continue(windows) => {
                if let Some(timeout) = args.timeout
                    && started.elapsed() >= timeout
                {
                    if cli.json {
                        print_wait_json(
                            "timeout",
                            args.check.remaining_percent,
                            windows,
                            None,
                            None,
                        )?;
                    } else {
                        println!("threshold not reached within {}", format_duration(timeout));
                    }
                    return Ok(ExitCode::SUCCESS);
                }

                let sleep_for = next_sleep(args.interval, args.timeout, started.elapsed());
                if cli.json {
                    print_wait_json(
                        "waiting",
                        args.check.remaining_percent,
                        windows,
                        None,
                        Some(sleep_for.as_secs()),
                    )?;
                } else {
                    print_wait_continue(args.check.remaining_percent, &windows, sleep_for);
                }
                tokio::time::sleep(sleep_for).await;
            }
        }
    }
}

fn next_sleep(interval: Duration, timeout: Option<Duration>, elapsed: Duration) -> Duration {
    match timeout.and_then(|timeout| timeout.checked_sub(elapsed)) {
        Some(remaining) => interval.min(remaining),
        None => interval,
    }
}

fn evaluate_wait(
    rate_limits: &RateLimitsOutput,
    window: WaitWindow,
    threshold: f64,
) -> WaitDecision {
    if matches!(rate_limits.status, RateLimitStatus::Unavailable) {
        return WaitDecision::Unavailable(
            rate_limits
                .reason
                .clone()
                .unwrap_or_else(|| "rate limits unavailable".to_string()),
        );
    }

    let windows = selected_wait_windows(rate_limits, window);
    if windows.is_empty() {
        return WaitDecision::Unavailable("no selected rate limit windows returned".to_string());
    }

    let triggered = windows
        .iter()
        .filter(|window| window.remaining_percent <= threshold)
        .cloned()
        .collect::<Vec<_>>();
    if triggered.is_empty() {
        WaitDecision::Continue(windows)
    } else {
        WaitDecision::Triggered(triggered)
    }
}

fn selected_wait_windows(
    rate_limits: &RateLimitsOutput,
    selection: WaitWindow,
) -> Vec<WaitObservedWindow> {
    let mut windows = Vec::new();
    for limit in &rate_limits.limits {
        if selection.includes_five_hour()
            && let Some(window) = &limit.five_hour
        {
            windows.push(wait_observed_window(&limit.scope, "5h", window));
        }
        if selection.includes_weekly()
            && let Some(window) = &limit.weekly
        {
            windows.push(wait_observed_window(&limit.scope, "weekly", window));
        }
        if selection.includes_monthly()
            && let Some(window) = &limit.monthly
        {
            windows.push(wait_observed_window(&limit.scope, "monthly", window));
        }
    }
    windows
}

fn wait_observed_window(
    scope: &str,
    label: &str,
    window: &RateLimitWindowOutput,
) -> WaitObservedWindow {
    WaitObservedWindow {
        scope: scope.to_string(),
        window: label.to_string(),
        remaining_percent: window.remaining_percent,
        used_percent: window.used_percent,
        reset_at: window.reset_at.clone(),
        reset_display: window.reset_display.clone(),
    }
}

impl WaitWindow {
    fn includes_five_hour(self) -> bool {
        matches!(self, WaitWindow::FiveHour | WaitWindow::Both)
    }

    fn includes_weekly(self) -> bool {
        matches!(self, WaitWindow::Weekly | WaitWindow::Both)
    }

    fn includes_monthly(self) -> bool {
        matches!(self, WaitWindow::Monthly | WaitWindow::Both)
    }
}

fn codex_output_from_config(config: &Config) -> CodexOutput {
    CodexOutput {
        codex_home: config.codex_home.display().to_string(),
        directory: config.cwd.display().to_string(),
        permissions: permissions_label(config),
        agents_md: agents_md_sources(config),
        collaboration_mode: "Default".to_string(),
        model: config
            .model
            .clone()
            .unwrap_or_else(|| "(default)".to_string()),
        model_details: model_details(config),
        model_provider: config.model_provider_id.clone(),
    }
}

fn auth_output_from_account_state(
    account: Option<&ProviderAccount>,
    requires_openai_auth: bool,
) -> AuthOutput {
    match account {
        Some(ProviderAccount::ApiKey) => AuthOutput {
            status: AuthStatus::ApiKey,
            requires_openai_auth,
            email: None,
            plan_type: None,
        },
        Some(ProviderAccount::Chatgpt { email, plan_type }) => AuthOutput {
            status: AuthStatus::Chatgpt,
            requires_openai_auth,
            email: Some(email.clone()),
            plan_type: Some(plan_type_label(plan_type)),
        },
        Some(ProviderAccount::AmazonBedrock) => AuthOutput {
            status: AuthStatus::AmazonBedrock,
            requires_openai_auth,
            email: None,
            plan_type: None,
        },
        None if requires_openai_auth => AuthOutput {
            status: AuthStatus::Unauthenticated,
            requires_openai_auth,
            email: None,
            plan_type: None,
        },
        None => AuthOutput {
            status: AuthStatus::NotRequired,
            requires_openai_auth,
            email: None,
            plan_type: None,
        },
    }
}

async fn load_rate_limits(base_url: &str, auth: Option<&CodexAuth>) -> RateLimitsOutput {
    let Some(auth) = auth else {
        return unavailable("codex account authentication required");
    };

    if !auth.uses_codex_backend() {
        return unavailable("chatgpt authentication required");
    }

    let client = match BackendClient::from_auth(base_url.to_string(), auth) {
        Ok(client) => client,
        Err(_) => return unavailable("failed to construct backend client"),
    };

    match client.get_rate_limits_many().await {
        Ok(snapshots) if snapshots.is_empty() => unavailable("no rate limit snapshots returned"),
        Ok(snapshots) => RateLimitsOutput {
            status: RateLimitStatus::Available,
            reason: None,
            limits: normalize_rate_limits(snapshots),
        },
        Err(err) => unavailable(rate_limit_error_reason(&err)),
    }
}

async fn load_token_usage(
    base_url: &str,
    auth: Option<&CodexAuth>,
    view: UsageView,
) -> UsageOutput {
    let Some(auth) = auth else {
        return usage_unavailable(
            view,
            "codex account authentication required to read token usage",
        );
    };

    if !auth.uses_codex_backend() {
        return usage_unavailable(view, "chatgpt authentication required to read token usage");
    }

    let client = match BackendClient::from_auth(base_url.to_string(), auth) {
        Ok(client) => client,
        Err(_) => return usage_unavailable(view, "failed to construct backend client"),
    };

    match client.get_token_usage_profile().await {
        Ok(profile) => normalize_token_usage(profile, view),
        Err(err) => usage_unavailable(view, token_usage_error_reason(&err)),
    }
}

fn usage_unavailable(view: UsageView, reason: impl Into<String>) -> UsageOutput {
    UsageOutput {
        status: UsageStatus::Unavailable,
        reason: Some(reason.into()),
        view,
        summary: None,
        daily_usage_buckets: None,
    }
}

fn token_usage_error_reason(err: &anyhow::Error) -> &'static str {
    let message = err.to_string();
    if message.contains("401") || message.contains("Unauthorized") {
        "authentication failed while reading token usage"
    } else {
        "failed to fetch token usage"
    }
}

fn normalize_token_usage(profile: TokenUsageProfile, view: UsageView) -> UsageOutput {
    let stats = profile.stats;
    UsageOutput {
        status: UsageStatus::Available,
        reason: None,
        view,
        summary: Some(usage_summary_output(&stats)),
        daily_usage_buckets: stats.daily_usage_buckets.map(normalize_usage_daily_buckets),
    }
}

fn usage_summary_output(stats: &TokenUsageProfileStats) -> UsageSummaryOutput {
    UsageSummaryOutput {
        lifetime_tokens: stats.lifetime_tokens,
        peak_daily_tokens: stats.peak_daily_tokens,
        longest_running_turn_sec: stats.longest_running_turn_sec,
        current_streak_days: stats.current_streak_days,
        longest_streak_days: stats.longest_streak_days,
    }
}

fn normalize_usage_daily_buckets(
    buckets: Vec<TokenUsageProfileDailyBucket>,
) -> Vec<UsageDailyBucketOutput> {
    let mut buckets = buckets
        .into_iter()
        .map(|bucket| UsageDailyBucketOutput {
            start_date: bucket.start_date,
            tokens: bucket.tokens,
        })
        .collect::<Vec<_>>();
    buckets.sort_by(|left, right| left.start_date.cmp(&right.start_date));
    buckets
}

fn unavailable(reason: impl Into<String>) -> RateLimitsOutput {
    RateLimitsOutput {
        status: RateLimitStatus::Unavailable,
        reason: Some(reason.into()),
        limits: Vec::new(),
    }
}

fn rate_limit_error_reason(err: &anyhow::Error) -> &'static str {
    let message = err.to_string();
    if message.contains("401") || message.contains("Unauthorized") {
        "authentication failed while reading rate limits"
    } else {
        "failed to fetch rate limits"
    }
}

fn normalize_rate_limits(snapshots: Vec<RateLimitSnapshot>) -> Vec<RateLimitOutput> {
    let mut by_limit_id = BTreeMap::new();
    for snapshot in snapshots {
        let limit_id = snapshot
            .limit_id
            .clone()
            .unwrap_or_else(|| "codex".to_string());
        by_limit_id.insert(limit_id, snapshot);
    }

    let mut additional_index = 0;
    by_limit_id
        .into_iter()
        .map(|(limit_id, snapshot)| {
            let scope = if limit_id == "codex" {
                "codex".to_string()
            } else {
                additional_index += 1;
                format!("additional_{additional_index}")
            };
            let mut output = RateLimitOutput {
                scope,
                plan_type: snapshot.plan_type.as_ref().map(plan_type_label),
                five_hour: None,
                weekly: None,
                monthly: None,
            };
            assign_rate_limit_window(
                &mut output,
                snapshot.primary.as_ref(),
                WindowFallback::Primary,
            );
            assign_rate_limit_window(
                &mut output,
                snapshot.secondary.as_ref(),
                WindowFallback::Secondary,
            );
            output
        })
        .collect()
}

#[derive(Clone, Copy)]
enum RateLimitWindowKind {
    FiveHour,
    Weekly,
    Monthly,
}

#[derive(Clone, Copy)]
enum WindowFallback {
    Primary,
    Secondary,
}

fn assign_rate_limit_window(
    output: &mut RateLimitOutput,
    window: Option<&RateLimitWindow>,
    fallback: WindowFallback,
) {
    let Some(window) = window else {
        return;
    };
    let value = window_output(window);
    match window_kind(window.window_minutes).unwrap_or_else(|| fallback_window_kind(fallback)) {
        RateLimitWindowKind::FiveHour => output.five_hour = Some(value),
        RateLimitWindowKind::Weekly => output.weekly = Some(value),
        RateLimitWindowKind::Monthly => output.monthly = Some(value),
    }
}

fn fallback_window_kind(fallback: WindowFallback) -> RateLimitWindowKind {
    match fallback {
        WindowFallback::Primary => RateLimitWindowKind::FiveHour,
        WindowFallback::Secondary => RateLimitWindowKind::Weekly,
    }
}

fn window_kind(window_minutes: Option<i64>) -> Option<RateLimitWindowKind> {
    let minutes = window_minutes?.max(0);
    if is_approximate_window(minutes, 5 * 60) {
        Some(RateLimitWindowKind::FiveHour)
    } else if is_approximate_window(minutes, 7 * 24 * 60) {
        Some(RateLimitWindowKind::Weekly)
    } else if is_approximate_window(minutes, 30 * 24 * 60) {
        Some(RateLimitWindowKind::Monthly)
    } else {
        None
    }
}

fn is_approximate_window(minutes: i64, expected_minutes: i64) -> bool {
    let minutes = minutes as f64;
    let expected_minutes = expected_minutes as f64;
    minutes >= expected_minutes * 0.95 && minutes <= expected_minutes * 1.05
}

fn window_output(window: &RateLimitWindow) -> RateLimitWindowOutput {
    let used_percent = window.used_percent.clamp(0.0, 100.0);
    RateLimitWindowOutput {
        remaining_percent: 100.0 - used_percent,
        used_percent,
        reset_at: window
            .resets_at
            .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
            .map(|dt| dt.to_rfc3339()),
        reset_display: window.resets_at.and_then(local_reset_display),
        window_minutes: window.window_minutes,
    }
}

fn local_reset_display(seconds: i64) -> Option<String> {
    let utc = chrono::DateTime::from_timestamp(seconds, 0)?;
    let local = utc.with_timezone(&chrono::Local);
    let now = chrono::Local::now();
    if local.date_naive() == now.date_naive() {
        Some(local.format("%H:%M").to_string())
    } else {
        Some(local.format("%H:%M on %d %b").to_string())
    }
}

fn model_details(config: &Config) -> Vec<String> {
    let mut details = Vec::new();
    if let Some(effort) = &config.model_reasoning_effort {
        details.push(format!("reasoning {effort}"));
    }
    if let Some(summary) = &config.model_reasoning_summary {
        details.push(format!("summaries {summary}"));
    }
    details
}

fn permissions_label(config: &Config) -> String {
    let permission_profile = config.permissions.effective_permission_profile();
    let active_permission_profile = config.permissions.active_permission_profile();
    let approval_policy = config.permissions.approval_policy.value();
    let approval = status_approval_label(
        approval_policy,
        config.approvals_reviewer,
        &approval_policy.to_string(),
    );
    let workspace_roots = config.effective_workspace_roots();
    let sandbox = status_permission_summary(&permission_profile, config, &workspace_roots);
    let workspace_root_suffix = workspace_root_suffix(&workspace_roots, config.cwd.as_path());

    status_permissions_label(
        active_permission_profile.as_ref(),
        &permission_profile,
        approval_policy,
        &sandbox,
        &approval,
        workspace_root_suffix.as_deref(),
    )
}

fn status_permission_summary(
    permission_profile: &PermissionProfile,
    config: &Config,
    workspace_roots: &[codex_utils_absolute_path::AbsolutePathBuf],
) -> String {
    let summary = summarize_permission_profile(permission_profile, &config.cwd, workspace_roots);
    if let Some(details) = summary.strip_prefix("read-only") {
        if details.contains("(network access enabled)") {
            return "read-only with network access".to_string();
        }
        return "read-only".to_string();
    }
    if let Some(details) = summary.strip_prefix("workspace-write") {
        if details.contains("(network access enabled)") {
            return "workspace with network access".to_string();
        }
        return "workspace".to_string();
    }
    if summary == "custom permissions (network access enabled)" {
        return "custom permissions with network access".to_string();
    }
    summary
}

fn workspace_root_suffix(
    workspace_roots: &[codex_utils_absolute_path::AbsolutePathBuf],
    cwd: &Path,
) -> Option<String> {
    let extra_roots = workspace_roots
        .iter()
        .filter(|root| root.as_path() != cwd)
        .map(|root| root.to_string_lossy().to_string())
        .collect::<Vec<_>>();
    if extra_roots.is_empty() {
        None
    } else {
        Some(format!(" [{}]", extra_roots.join(", ")))
    }
}

fn status_permissions_label(
    active_permission_profile: Option<&ActivePermissionProfile>,
    permission_profile: &PermissionProfile,
    approval_policy: AskForApproval,
    sandbox: &str,
    approval: &str,
    workspace_root_suffix: Option<&str>,
) -> String {
    let active_id = active_permission_profile.map(|active| active.id.as_str());
    match active_id {
        Some(BUILT_IN_PERMISSION_PROFILE_READ_ONLY) => {
            let label = if sandbox == "read-only with network access" {
                "Read Only with network access"
            } else {
                "Read Only"
            };
            return format!("{label} ({approval})");
        }
        Some(BUILT_IN_PERMISSION_PROFILE_WORKSPACE) => match sandbox {
            "workspace" => {
                return format!(
                    "Workspace{} ({approval})",
                    workspace_root_suffix.unwrap_or("")
                );
            }
            "workspace with network access" => {
                return format!(
                    "Workspace with network access{} ({approval})",
                    workspace_root_suffix.unwrap_or("")
                );
            }
            _ => {}
        },
        Some(BUILT_IN_PERMISSION_PROFILE_DANGER_FULL_ACCESS)
            if permission_profile == &PermissionProfile::Disabled =>
        {
            return if approval_policy == AskForApproval::Never {
                "Full Access".to_string()
            } else {
                format!("No Sandbox ({approval})")
            };
        }
        Some(id) => {
            let sandbox = decorate_workspace_sandbox_label(sandbox, workspace_root_suffix);
            return format!("Profile {id} ({sandbox}, {approval})");
        }
        None => {}
    }

    if sandbox == "read-only" {
        return format!("Read Only ({approval})");
    }
    if approval_policy == AskForApproval::OnRequest && sandbox == "workspace" {
        return format!(
            "Workspace{} ({approval})",
            workspace_root_suffix.unwrap_or("")
        );
    }
    if approval_policy == AskForApproval::Never
        && permission_profile == &PermissionProfile::Disabled
    {
        return "Full Access".to_string();
    }
    let sandbox = decorate_workspace_sandbox_label(sandbox, workspace_root_suffix);
    format!("Custom ({sandbox}, {approval})")
}

fn decorate_workspace_sandbox_label(sandbox: &str, workspace_root_suffix: Option<&str>) -> String {
    match workspace_root_suffix {
        Some(suffix) if sandbox.starts_with("workspace") => format!("{sandbox}{suffix}"),
        _ => sandbox.to_string(),
    }
}

fn status_approval_label(
    approval_policy: AskForApproval,
    approvals_reviewer: ApprovalsReviewer,
    approval: &str,
) -> String {
    if approval_policy == AskForApproval::OnRequest {
        return match approvals_reviewer {
            ApprovalsReviewer::AutoReview => "Approve for me".to_string(),
            ApprovalsReviewer::User => "Ask for approval".to_string(),
        };
    }

    approval.to_string()
}

fn agents_md_sources(config: &Config) -> Vec<String> {
    let mut sources = BTreeSet::new();
    let user_agents = config.codex_home.join("AGENTS.md");
    if user_agents.as_path().is_file() {
        sources.insert(user_agents.to_string_lossy().to_string());
    }

    for dir in agents_md_search_dirs(config.cwd.as_path()) {
        for name in agents_md_candidate_filenames(config) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                sources.insert(candidate.display().to_string());
                break;
            }
        }
    }

    sources.into_iter().collect()
}

fn agents_md_search_dirs(cwd: &Path) -> Vec<&Path> {
    let mut dirs = Vec::new();
    let mut project_root = None;
    for ancestor in cwd.ancestors() {
        if ancestor.join(".git").exists() {
            project_root = Some(ancestor);
            break;
        }
    }

    let Some(root) = project_root else {
        return vec![cwd];
    };

    for ancestor in cwd.ancestors() {
        dirs.push(ancestor);
        if ancestor == root {
            break;
        }
    }
    dirs.reverse();
    dirs
}

fn agents_md_candidate_filenames(config: &Config) -> Vec<&str> {
    let mut names = vec!["AGENTS.override.md", "AGENTS.md"];
    for candidate in &config.project_doc_fallback_filenames {
        if !candidate.is_empty() && !names.contains(&candidate.as_str()) {
            names.push(candidate.as_str());
        }
    }
    names
}

fn plan_type_label(plan_type: &PlanType) -> String {
    match plan_type {
        PlanType::Free => "free",
        PlanType::Go => "go",
        PlanType::Plus => "plus",
        PlanType::Pro => "pro",
        PlanType::ProLite => "pro_lite",
        PlanType::Team => "team",
        PlanType::SelfServeBusinessUsageBased => "self_serve_business_usage_based",
        PlanType::Business => "business",
        PlanType::EnterpriseCbpUsageBased => "enterprise_cbp_usage_based",
        PlanType::Enterprise => "enterprise",
        PlanType::Edu => "edu",
        PlanType::Unknown => "unknown",
    }
    .to_string()
}

fn print_human(output: &StatusOutput) {
    println!("Codex status");
    println!("  Model              {}", model_status_label(&output.codex));
    println!("  Directory          {}", output.codex.directory);
    println!("  Codex home         {}", output.codex.codex_home);
    println!("  Permissions        {}", output.codex.permissions);
    println!(
        "  Agents.md          {}",
        agents_md_status_label(&output.codex)
    );
    println!("  Account            {}", auth_status_label(&output.auth));
    println!("  Collaboration mode {}", output.codex.collaboration_mode);
    println!();
    println!("Rate limits");
    match output.rate_limits.status {
        RateLimitStatus::Unavailable => {
            let reason = output
                .rate_limits
                .reason
                .as_deref()
                .unwrap_or("unavailable");
            println!("  unavailable     {reason}");
        }
        RateLimitStatus::Available => {
            if output.rate_limits.limits.is_empty()
                || !output
                    .rate_limits
                    .limits
                    .iter()
                    .any(limit_has_displayable_window)
            {
                println!("  unavailable     no displayable limits");
                return;
            }
            for limit in &output.rate_limits.limits {
                if limit.scope != "codex" {
                    println!("  {} limit:", limit.scope);
                }
                print_limit_window("5h limit", limit.five_hour.as_ref());
                print_limit_window("Weekly limit", limit.weekly.as_ref());
                print_limit_window("Monthly limit", limit.monthly.as_ref());
            }
        }
    }
}

fn print_usage_human(output: &UsageOutput) {
    println!("Token activity   last 12 months");
    match output.status {
        UsageStatus::Unavailable => {
            let reason = output.reason.as_deref().unwrap_or("unavailable");
            println!("  unavailable     {reason}");
        }
        UsageStatus::Available => {
            if let Some(summary) = &output.summary {
                println!("  {}", usage_summary_line(summary));
            }
            match output.daily_usage_buckets.as_deref() {
                Some(buckets) => {
                    for line in token_activity_lines(
                        output.view,
                        buckets,
                        chrono::Local::now().date_naive(),
                    ) {
                        println!("{line}");
                    }
                }
                None => println!("  Token activity history unavailable"),
            }
        }
    }
}

fn limit_has_displayable_window(limit: &RateLimitOutput) -> bool {
    limit.five_hour.is_some() || limit.weekly.is_some() || limit.monthly.is_some()
}

fn model_status_label(codex: &CodexOutput) -> String {
    if codex.model_details.is_empty() {
        codex.model.clone()
    } else {
        format!("{} ({})", codex.model, codex.model_details.join(", "))
    }
}

fn agents_md_status_label(codex: &CodexOutput) -> String {
    if codex.agents_md.is_empty() {
        "-".to_string()
    } else {
        codex.agents_md.join(", ")
    }
}

fn auth_status_label(auth: &AuthOutput) -> String {
    let status = match auth.status {
        AuthStatus::Chatgpt => "ChatGPT",
        AuthStatus::ApiKey => "API key",
        AuthStatus::AmazonBedrock => "Amazon Bedrock",
        AuthStatus::Unauthenticated => "unauthenticated",
        AuthStatus::NotRequired => "not required",
        AuthStatus::Unavailable => "unavailable",
    };
    match (auth.email.as_deref(), auth.plan_type.as_deref()) {
        (Some(email), Some(plan)) => format!("{email} ({plan})"),
        (Some(email), None) => email.to_string(),
        (None, Some(plan)) => format!("{status} ({plan})"),
        (None, None) => status.to_string(),
    }
}

fn print_limit_window(label: &str, window: Option<&RateLimitWindowOutput>) {
    let Some(window) = window else {
        return;
    };
    let reset = window.reset_display.as_deref().unwrap_or("-");
    println!(
        "  {:<18} {} {:>3.0}% left (resets {})",
        label,
        percent_bar(window.remaining_percent),
        window.remaining_percent,
        reset
    );
}

fn format_check_ok(threshold: f64, windows: &[WaitObservedWindow]) -> String {
    if let Some(lowest) = lowest_remaining_window(windows) {
        format!(
            "rate limit check ok: lowest selected limit is {} {} limit {:.0}% left > {:.0}%{}",
            lowest.scope,
            lowest.window,
            lowest.remaining_percent,
            threshold,
            wait_reset_suffix(lowest)
        ) + "\n"
    } else {
        String::new()
    }
}

fn print_wait_triggered(threshold: f64, windows: &[WaitObservedWindow]) {
    print!("{}", format_wait_triggered(threshold, windows));
}

fn format_wait_triggered(threshold: f64, windows: &[WaitObservedWindow]) -> String {
    let mut output = String::new();
    for window in windows {
        output.push_str(&format!(
            "rate limit threshold reached: {} {} limit {:.0}% left <= {:.0}%{}",
            window.scope,
            window.window,
            window.remaining_percent,
            threshold,
            wait_reset_suffix(window)
        ));
        output.push('\n');
    }
    output
}

fn print_wait_continue(threshold: f64, windows: &[WaitObservedWindow], sleep_for: Duration) {
    if let Some(lowest) = lowest_remaining_window(windows) {
        println!(
            "waiting: lowest selected limit is {} {} limit {:.0}% left (threshold {:.0}%, next poll in {}){}",
            lowest.scope,
            lowest.window,
            lowest.remaining_percent,
            threshold,
            format_duration(sleep_for),
            wait_reset_suffix(lowest)
        );
    }
}

fn print_wait_json(
    status: &str,
    threshold: f64,
    windows: Vec<WaitObservedWindow>,
    reason: Option<String>,
    next_poll_seconds: Option<u64>,
) -> anyhow::Result<()> {
    print!(
        "{}",
        format_wait_json(status, threshold, windows, reason, next_poll_seconds)?
    );
    Ok(())
}

fn format_wait_json(
    status: &str,
    threshold: f64,
    windows: Vec<WaitObservedWindow>,
    reason: Option<String>,
    next_poll_seconds: Option<u64>,
) -> anyhow::Result<String> {
    let output = WaitEventOutput {
        status: status.to_string(),
        threshold_remaining_percent: threshold,
        windows,
        reason,
        next_poll_seconds,
    };
    Ok(format!("{}\n", serde_json::to_string(&output)?))
}

fn lowest_remaining_window(windows: &[WaitObservedWindow]) -> Option<&WaitObservedWindow> {
    windows
        .iter()
        .min_by(|left, right| left.remaining_percent.total_cmp(&right.remaining_percent))
}

fn wait_reset_suffix(window: &WaitObservedWindow) -> String {
    window
        .reset_display
        .as_ref()
        .map(|reset| format!(" (resets {reset})"))
        .unwrap_or_default()
}

fn parse_percent_arg(raw: &str) -> Result<f64, String> {
    let value = raw
        .trim()
        .strip_suffix('%')
        .unwrap_or(raw.trim())
        .parse::<f64>()
        .map_err(|_| "expected a percentage from 0 to 100".to_string())?;
    if (0.0..=100.0).contains(&value) {
        Ok(value)
    } else {
        Err("expected a percentage from 0 to 100".to_string())
    }
}

fn parse_duration_arg(raw: &str) -> Result<Duration, String> {
    let value = raw.trim().to_ascii_lowercase();
    if value.is_empty() {
        return Err("expected a duration".to_string());
    }

    let suffixes = [
        ("milliseconds", 1_u64),
        ("millisecond", 1),
        ("millis", 1),
        ("ms", 1),
        ("minutes", 60_000),
        ("minute", 60_000),
        ("mins", 60_000),
        ("min", 60_000),
        ("hours", 3_600_000),
        ("hour", 3_600_000),
        ("hrs", 3_600_000),
        ("hr", 3_600_000),
        ("seconds", 1_000),
        ("second", 1_000),
        ("secs", 1_000),
        ("sec", 1_000),
        ("h", 3_600_000),
        ("m", 60_000),
        ("s", 1_000),
    ];

    let (number, multiplier) = suffixes
        .iter()
        .find_map(|(suffix, multiplier)| {
            value
                .strip_suffix(suffix)
                .map(|number| (number.trim(), *multiplier))
        })
        .unwrap_or((value.as_str(), 1_000));

    let amount = number
        .parse::<u64>()
        .map_err(|_| "expected a positive duration such as 60s or 1m".to_string())?;
    let millis = amount
        .checked_mul(multiplier)
        .ok_or_else(|| "duration is too large".to_string())?;
    if millis == 0 {
        return Err("expected a positive duration".to_string());
    }
    Ok(Duration::from_millis(millis))
}

fn format_duration(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis < 1_000 {
        return format!("{millis}ms");
    }

    let seconds = duration.as_secs();
    if seconds.is_multiple_of(3_600) {
        format!("{}h", seconds / 3_600)
    } else if seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

fn usage_summary_line(summary: &UsageSummaryOutput) -> String {
    let streak = match (summary.current_streak_days, summary.longest_streak_days) {
        (Some(current), Some(best)) => format!("{current}d (best {best}d)"),
        (Some(current), None) => format!("{current}d"),
        (None, Some(best)) => format!("- (best {best}d)"),
        (None, None) => "-".to_string(),
    };
    format!(
        "Lifetime {} · Peak {} · Streak {} · Longest task {}",
        format_optional_tokens(summary.lifetime_tokens),
        format_optional_tokens(summary.peak_daily_tokens),
        streak,
        format_optional_seconds(summary.longest_running_turn_sec)
    )
}

fn token_activity_lines(
    view: UsageView,
    buckets: &[UsageDailyBucketOutput],
    today: chrono::NaiveDate,
) -> Vec<String> {
    let weeks = activity_weeks(today);
    let values = activity_values(buckets, &weeks, today);
    match view {
        UsageView::Daily => daily_activity_lines(&weeks, &values),
        UsageView::Weekly | UsageView::Cumulative => {
            aggregate_activity_lines(view, &weeks, &values)
        }
    }
}

fn activity_weeks(today: chrono::NaiveDate) -> Vec<chrono::NaiveDate> {
    use chrono::Datelike;

    let days_until_saturday = 6_i64 - i64::from(today.weekday().num_days_from_sunday());
    let end = today + chrono::Duration::days(days_until_saturday);
    let start = end - chrono::Duration::days((52 * 7 - 1) as i64);
    (0..52)
        .map(|week| start + chrono::Duration::days((week * 7) as i64))
        .collect()
}

fn activity_values(
    buckets: &[UsageDailyBucketOutput],
    weeks: &[chrono::NaiveDate],
    today: chrono::NaiveDate,
) -> Vec<Option<i64>> {
    let by_date = buckets
        .iter()
        .filter_map(|bucket| {
            let date = chrono::NaiveDate::parse_from_str(&bucket.start_date, "%Y-%m-%d").ok()?;
            Some((date, bucket.tokens.max(0)))
        })
        .collect::<BTreeMap<_, _>>();

    weeks
        .iter()
        .flat_map(|week_start| {
            (0..7).map(|day| {
                let date = *week_start + chrono::Duration::days(day);
                if date > today {
                    None
                } else {
                    Some(*by_date.get(&date).unwrap_or(&0))
                }
            })
        })
        .collect()
}

fn daily_activity_lines(weeks: &[chrono::NaiveDate], values: &[Option<i64>]) -> Vec<String> {
    let levels = activity_levels(values);
    let mut lines = vec![month_header(weeks)];
    for row in 0..7 {
        let mut line = format!(" {:<2}", weekday_abbrev(row));
        for column in 0..52 {
            let index = column * 7 + row;
            line.push(' ');
            line.push(activity_glyph(levels[index], true));
        }
        lines.push(line);
    }
    lines.push("     · none  ░ low  ▒ medium  ▓ high  █ peak".to_string());
    lines
}

fn aggregate_activity_lines(
    view: UsageView,
    weeks: &[chrono::NaiveDate],
    values: &[Option<i64>],
) -> Vec<String> {
    let mut totals = weekly_totals(values);
    if view == UsageView::Cumulative {
        let mut running = 0;
        for total in &mut totals {
            running += *total;
            *total = running;
        }
    }
    let levels = activity_levels(&totals.iter().copied().map(Some).collect::<Vec<_>>());
    let mut line = "     ".to_string();
    for level in levels {
        line.push(' ');
        line.push(activity_glyph(level, false));
    }
    let label = match view {
        UsageView::Weekly => "Weekly total",
        UsageView::Cumulative => "Running total",
        UsageView::Daily => unreachable!(),
    };
    let peak = totals.iter().copied().max().unwrap_or(0);
    vec![
        month_header(weeks),
        line,
        format!("     {label} · top {}", format_tokens_compact(peak)),
    ]
}

fn weekly_totals(values: &[Option<i64>]) -> Vec<i64> {
    values
        .chunks(7)
        .map(|week| week.iter().flatten().sum::<i64>())
        .collect()
}

fn activity_levels(values: &[Option<i64>]) -> Vec<Option<usize>> {
    let peak = values.iter().flatten().copied().max().unwrap_or(0);
    values
        .iter()
        .map(|value| {
            let value = (*value)?;
            if value == 0 || peak == 0 {
                Some(0)
            } else {
                Some(((value as f64 / peak as f64) * 4.0).ceil() as usize)
            }
        })
        .collect()
}

fn activity_glyph(level: Option<usize>, daily: bool) -> char {
    match level {
        None => ' ',
        Some(0) if daily => '·',
        Some(0) => ' ',
        Some(1) => '░',
        Some(2) => '▒',
        Some(3) => '▓',
        Some(_) => '█',
    }
}

fn month_header(weeks: &[chrono::NaiveDate]) -> String {
    use chrono::Datelike;

    let mut line = "    ".to_string();
    let mut previous_month = None;
    for week in weeks {
        line.push(' ');
        if previous_month != Some(week.month()) {
            line.push_str(month_abbrev(week.month()));
            previous_month = Some(week.month());
        } else {
            line.push(' ');
        }
    }
    line
}

fn month_abbrev(month: u32) -> &'static str {
    match month {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => "",
    }
}

fn weekday_abbrev(row: usize) -> &'static str {
    match row {
        0 => "Su",
        1 => "Mo",
        2 => "Tu",
        3 => "We",
        4 => "Th",
        5 => "Fr",
        6 => "Sa",
        _ => "",
    }
}

fn format_optional_tokens(value: Option<i64>) -> String {
    value
        .map(format_tokens_compact)
        .unwrap_or_else(|| "-".to_string())
}

fn format_tokens_compact(value: i64) -> String {
    let value = value.max(0);
    if value >= 1_000_000_000 {
        format_compact_number(value, 1_000_000_000, "B")
    } else if value >= 1_000_000 {
        format_compact_number(value, 1_000_000, "M")
    } else if value >= 1_000 {
        format_compact_number(value, 1_000, "K")
    } else {
        value.to_string()
    }
}

fn format_compact_number(value: i64, divisor: i64, suffix: &str) -> String {
    let scaled = value as f64 / divisor as f64;
    if scaled >= 100.0 || (scaled.fract() - 0.0).abs() < f64::EPSILON {
        format!("{scaled:.0}{suffix}")
    } else {
        format!("{scaled:.1}{suffix}")
    }
}

fn format_optional_seconds(value: Option<i64>) -> String {
    let Some(value) = value else {
        return "-".to_string();
    };
    let seconds = value.max(0);
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    if hours > 0 && minutes > 0 {
        format!("{hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{seconds}s")
    }
}

fn percent_bar(percent: f64) -> String {
    let width = 20;
    let filled = ((percent.clamp(0.0, 100.0) / 100.0) * width as f64).round() as usize;
    let empty = width - filled.min(width);
    format!(
        "[{}{}]",
        "\u{2588}".repeat(filled.min(width)),
        "\u{2591}".repeat(empty)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::protocol::CreditsSnapshot;
    use codex_protocol::protocol::SpendControlLimitSnapshot;

    fn snapshot(
        limit_id: Option<&str>,
        primary: Option<f64>,
        secondary: Option<f64>,
    ) -> RateLimitSnapshot {
        snapshot_with_window_minutes(limit_id, primary, Some(300), secondary, Some(10_080))
    }

    fn snapshot_with_window_minutes(
        limit_id: Option<&str>,
        primary: Option<f64>,
        primary_window_minutes: Option<i64>,
        secondary: Option<f64>,
        secondary_window_minutes: Option<i64>,
    ) -> RateLimitSnapshot {
        RateLimitSnapshot {
            limit_id: limit_id.map(str::to_string),
            limit_name: None,
            primary: primary.map(|used_percent| RateLimitWindow {
                used_percent,
                window_minutes: primary_window_minutes,
                resets_at: Some(1_700_000_000),
            }),
            secondary: secondary.map(|used_percent| RateLimitWindow {
                used_percent,
                window_minutes: secondary_window_minutes,
                resets_at: Some(1_700_360_000),
            }),
            credits: None::<CreditsSnapshot>,
            individual_limit: None::<SpendControlLimitSnapshot>,
            plan_type: Some(PlanType::Plus),
            rate_limit_reached_type: None,
        }
    }

    fn token_usage_profile() -> TokenUsageProfile {
        TokenUsageProfile {
            stats: TokenUsageProfileStats {
                lifetime_tokens: Some(19_100_000_000),
                peak_daily_tokens: Some(912_000_000),
                longest_running_turn_sec: Some(42_240),
                current_streak_days: Some(1),
                longest_streak_days: Some(38),
                daily_usage_buckets: Some(vec![
                    TokenUsageProfileDailyBucket {
                        start_date: "2026-06-15".to_string(),
                        tokens: 12_000,
                    },
                    TokenUsageProfileDailyBucket {
                        start_date: "2026-06-14".to_string(),
                        tokens: 24_000,
                    },
                ]),
            },
        }
    }

    #[test]
    fn normalizes_default_limit_id_and_remaining_percentages() {
        let limits = normalize_rate_limits(vec![snapshot(None, Some(25.0), Some(40.0))]);

        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].scope, "codex");
        assert_eq!(
            limits[0].five_hour.as_ref().unwrap().remaining_percent,
            75.0
        );
        assert_eq!(limits[0].weekly.as_ref().unwrap().remaining_percent, 60.0);
    }

    #[test]
    fn normalizes_monthly_primary_window_by_duration() {
        let limits = normalize_rate_limits(vec![snapshot_with_window_minutes(
            None,
            Some(27.0),
            Some(43_200),
            None,
            None,
        )]);

        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].scope, "codex");
        assert!(limits[0].five_hour.is_none());
        assert!(limits[0].weekly.is_none());
        assert_eq!(limits[0].monthly.as_ref().unwrap().remaining_percent, 73.0);
    }

    #[test]
    fn api_key_auth_has_fixed_unavailable_reason() {
        let output = unavailable("chatgpt authentication required");

        assert!(matches!(output.status, RateLimitStatus::Unavailable));
        assert_eq!(
            output.reason.as_deref(),
            Some("chatgpt authentication required")
        );
    }

    #[test]
    fn usage_unavailable_has_fixed_reason_without_payload() {
        let output = usage_unavailable(
            UsageView::Daily,
            "chatgpt authentication required to read token usage",
        );
        let json = serde_json::to_string(&output).unwrap();

        assert!(matches!(output.status, UsageStatus::Unavailable));
        assert_eq!(
            output.reason.as_deref(),
            Some("chatgpt authentication required to read token usage")
        );
        assert!(output.summary.is_none());
        assert!(output.daily_usage_buckets.is_none());
        assert!(!json.contains("Authorization"));
        assert!(!json.contains("github_pat_"));
    }

    #[test]
    fn unauthenticated_auth_has_fixed_unavailable_reason() {
        let output = unavailable("codex account authentication required");

        assert!(matches!(output.status, RateLimitStatus::Unavailable));
        assert_eq!(
            output.reason.as_deref(),
            Some("codex account authentication required")
        );
    }

    #[test]
    fn auth_output_maps_api_key_without_plan_or_secret() {
        let output = auth_output_from_account_state(Some(&ProviderAccount::ApiKey), true);

        assert!(matches!(output.status, AuthStatus::ApiKey));
        assert!(output.requires_openai_auth);
        assert_eq!(output.email, None);
        assert_eq!(output.plan_type, None);
    }

    #[test]
    fn auth_output_maps_unauthenticated() {
        let output = auth_output_from_account_state(None, true);

        assert!(matches!(output.status, AuthStatus::Unauthenticated));
        assert!(output.requires_openai_auth);
        assert_eq!(output.email, None);
        assert_eq!(output.plan_type, None);
    }

    #[test]
    fn auth_output_maps_chatgpt_with_email_like_status() {
        let account = ProviderAccount::Chatgpt {
            email: "user@example.invalid".to_string(),
            plan_type: PlanType::Pro,
        };
        let output = auth_output_from_account_state(Some(&account), true);
        let json = serde_json::to_string(&output).unwrap();

        assert!(matches!(output.status, AuthStatus::Chatgpt));
        assert_eq!(output.email.as_deref(), Some("user@example.invalid"));
        assert_eq!(output.plan_type.as_deref(), Some("pro"));
        assert!(json.contains("user@example.invalid"));
    }

    #[test]
    fn wait_evaluation_triggers_only_selected_window() {
        let rate_limits = RateLimitsOutput {
            status: RateLimitStatus::Available,
            reason: None,
            limits: normalize_rate_limits(vec![snapshot(None, Some(10.0), Some(82.0))]),
        };

        let five_hour = evaluate_wait(&rate_limits, WaitWindow::FiveHour, 20.0);
        assert!(matches!(five_hour, WaitDecision::Continue(_)));

        let weekly = evaluate_wait(&rate_limits, WaitWindow::Weekly, 20.0);
        match weekly {
            WaitDecision::Triggered(windows) => {
                assert_eq!(windows.len(), 1);
                assert_eq!(windows[0].scope, "codex");
                assert_eq!(windows[0].window, "weekly");
                assert_eq!(windows[0].remaining_percent, 18.0);
            }
            other => panic!("expected weekly trigger, got {other:?}"),
        }
    }

    #[test]
    fn wait_evaluation_can_select_monthly_window() {
        let rate_limits = RateLimitsOutput {
            status: RateLimitStatus::Available,
            reason: None,
            limits: normalize_rate_limits(vec![snapshot_with_window_minutes(
                None,
                Some(88.0),
                Some(43_200),
                None,
                None,
            )]),
        };

        match evaluate_wait(&rate_limits, WaitWindow::Monthly, 15.0) {
            WaitDecision::Triggered(windows) => {
                assert_eq!(windows.len(), 1);
                assert_eq!(windows[0].scope, "codex");
                assert_eq!(windows[0].window, "monthly");
                assert_eq!(windows[0].remaining_percent, 12.0);
            }
            other => panic!("expected monthly trigger, got {other:?}"),
        }
    }

    #[test]
    fn wait_evaluation_reports_unavailable_reason() {
        let decision = evaluate_wait(
            &unavailable("chatgpt authentication required"),
            WaitWindow::Both,
            10.0,
        );

        assert_eq!(
            decision,
            WaitDecision::Unavailable("chatgpt authentication required".to_string())
        );
    }

    #[test]
    fn wait_evaluation_handles_missing_selected_windows() {
        let rate_limits = RateLimitsOutput {
            status: RateLimitStatus::Available,
            reason: None,
            limits: normalize_rate_limits(vec![snapshot(None, Some(10.0), None)]),
        };

        assert_eq!(
            evaluate_wait(&rate_limits, WaitWindow::Weekly, 10.0),
            WaitDecision::Unavailable("no selected rate limit windows returned".to_string())
        );
    }

    #[test]
    fn parses_wait_percentages() {
        assert_eq!(parse_percent_arg("10").unwrap(), 10.0);
        assert_eq!(parse_percent_arg("10%").unwrap(), 10.0);
        assert!(parse_percent_arg("-1").is_err());
        assert!(parse_percent_arg("101").is_err());
    }

    #[test]
    fn parses_wait_durations() {
        assert_eq!(parse_duration_arg("1").unwrap(), Duration::from_secs(1));
        assert_eq!(parse_duration_arg("60s").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_duration_arg("1m").unwrap(), Duration::from_secs(60));
        assert_eq!(
            parse_duration_arg("1 minute").unwrap(),
            Duration::from_secs(60)
        );
        assert_eq!(
            parse_duration_arg("1h").unwrap(),
            Duration::from_secs(3_600)
        );
        assert_eq!(
            parse_duration_arg("250ms").unwrap(),
            Duration::from_millis(250)
        );
        assert!(parse_duration_arg("0s").is_err());
    }

    #[test]
    fn parses_check_command_threshold_options() {
        let cli = Cli::try_parse_from([
            "cxst",
            "check",
            "--remaining-percent",
            "12%",
            "--window",
            "weekly",
        ])
        .unwrap();

        match cli.command {
            Some(Command::Check(args)) => {
                assert_eq!(args.remaining_percent, 12.0);
                assert_eq!(args.window, WaitWindow::Weekly);
            }
            other => panic!("expected check command, got {other:?}"),
        }
    }

    #[test]
    fn parses_check_command_monthly_window() {
        let cli = Cli::try_parse_from([
            "cxst",
            "check",
            "--remaining-percent",
            "25",
            "--window",
            "monthly",
        ])
        .unwrap();

        match cli.command {
            Some(Command::Check(args)) => {
                assert_eq!(args.remaining_percent, 25.0);
                assert_eq!(args.window, WaitWindow::Monthly);
            }
            other => panic!("expected check command, got {other:?}"),
        }
    }

    #[test]
    fn parses_usage_command_default_and_aliases() {
        let cli = Cli::try_parse_from(["cxst", "usage"]).unwrap();
        match cli.command {
            Some(Command::Usage(args)) => assert_eq!(args.view, UsageView::Daily),
            other => panic!("expected usage command, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["cxst", "usage", "week"]).unwrap();
        match cli.command {
            Some(Command::Usage(args)) => assert_eq!(args.view, UsageView::Weekly),
            other => panic!("expected usage command, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["cxst", "--json", "usage", "cumulative"]).unwrap();
        assert!(cli.json);
        match cli.command {
            Some(Command::Usage(args)) => assert_eq!(args.view, UsageView::Cumulative),
            other => panic!("expected usage command, got {other:?}"),
        }
    }

    #[test]
    fn normalizes_token_usage_profile_for_json() {
        let output = normalize_token_usage(token_usage_profile(), UsageView::Daily);
        let json = serde_json::to_value(&output).unwrap();

        assert_eq!(json["status"], "available");
        assert_eq!(json["reason"], serde_json::Value::Null);
        assert_eq!(json["view"], "daily");
        assert_eq!(json["summary"]["lifetimeTokens"], 19_100_000_000_i64);
        assert_eq!(json["summary"]["peakDailyTokens"], 912_000_000_i64);
        assert_eq!(json["summary"]["longestRunningTurnSec"], 42_240);
        assert_eq!(json["summary"]["currentStreakDays"], 1);
        assert_eq!(json["summary"]["longestStreakDays"], 38);
        assert_eq!(json["dailyUsageBuckets"][0]["startDate"], "2026-06-14");
        assert_eq!(json["dailyUsageBuckets"][0]["tokens"], 24_000);
        assert_eq!(json["dailyUsageBuckets"][1]["startDate"], "2026-06-15");
        assert_eq!(json["dailyUsageBuckets"][1]["tokens"], 12_000);
        assert!(json.get("auth").is_none());
        assert!(json.get("codex").is_none());
    }

    #[test]
    fn token_usage_history_can_be_missing() {
        let profile = TokenUsageProfile {
            stats: TokenUsageProfileStats {
                lifetime_tokens: None,
                peak_daily_tokens: None,
                longest_running_turn_sec: None,
                current_streak_days: None,
                longest_streak_days: None,
                daily_usage_buckets: None,
            },
        };

        let output = normalize_token_usage(profile, UsageView::Weekly);

        assert!(matches!(output.status, UsageStatus::Available));
        assert!(output.daily_usage_buckets.is_none());
        assert_eq!(
            usage_summary_line(output.summary.as_ref().unwrap()),
            "Lifetime - · Peak - · Streak - · Longest task -"
        );
    }

    #[test]
    fn formats_usage_summary_and_duration() {
        let summary = usage_summary_output(&token_usage_profile().stats);

        assert_eq!(
            usage_summary_line(&summary),
            "Lifetime 19.1B · Peak 912M · Streak 1d (best 38d) · Longest task 11h 44m"
        );
        assert_eq!(format_optional_seconds(Some(59)), "59s");
        assert_eq!(format_optional_seconds(Some(60)), "1m");
        assert_eq!(format_optional_seconds(Some(3_600)), "1h");
        assert_eq!(format_tokens_compact(1_500), "1.5K");
    }

    #[test]
    fn renders_daily_and_weekly_usage_lines() {
        let buckets = vec![
            UsageDailyBucketOutput {
                start_date: "2026-06-14".to_string(),
                tokens: 10,
            },
            UsageDailyBucketOutput {
                start_date: "2026-06-15".to_string(),
                tokens: 20,
            },
        ];
        let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 16).unwrap();

        let daily = token_activity_lines(UsageView::Daily, &buckets, today);
        assert!(daily[0].contains("Jun"));
        assert_eq!(daily.len(), 9);
        assert!(daily.iter().any(|line| line.starts_with(" Su")));
        assert!(daily.iter().any(|line| line.contains('█')));

        let weekly = token_activity_lines(UsageView::Weekly, &buckets, today);
        assert_eq!(weekly.len(), 3);
        assert!(weekly[2].contains("Weekly total"));
        assert!(weekly[2].contains("30"));
    }

    #[test]
    fn parses_check_command_all_window_alias() {
        let cli = Cli::try_parse_from([
            "cxst",
            "check",
            "--remaining-percent",
            "25",
            "--window",
            "all",
        ])
        .unwrap();

        match cli.command {
            Some(Command::Check(args)) => {
                assert_eq!(args.remaining_percent, 25.0);
                assert_eq!(args.window, WaitWindow::Both);
            }
            other => panic!("expected check command, got {other:?}"),
        }
    }

    #[test]
    fn check_with_mock_snapshot_exits_zero_when_above_threshold() {
        let args = CheckArgs {
            remaining_percent: 10.0,
            window: WaitWindow::Both,
        };
        let rate_limits = RateLimitsOutput {
            status: RateLimitStatus::Available,
            reason: None,
            limits: normalize_rate_limits(vec![snapshot(None, Some(20.0), Some(30.0))]),
        };

        let result = check_rate_limits(&rate_limits, &args, true).unwrap();
        let json: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();

        assert_eq!(result.exit_code, 0);
        assert_eq!(json["status"], "ok");
        assert_eq!(json["thresholdRemainingPercent"], 10.0);
        assert_eq!(json["windows"].as_array().unwrap().len(), 2);
        assert!(!result.stdout.contains("email"));
        assert!(!result.stdout.contains("codexHome"));
        assert!(!result.stdout.contains("directory"));
        assert!(!result.stdout.contains("agentsMd"));
        assert!(!result.stdout.contains("token"));
    }

    #[test]
    fn check_with_mock_snapshot_exits_one_when_threshold_is_reached() {
        let args = CheckArgs {
            remaining_percent: 20.0,
            window: WaitWindow::Weekly,
        };
        let rate_limits = RateLimitsOutput {
            status: RateLimitStatus::Available,
            reason: None,
            limits: normalize_rate_limits(vec![snapshot(None, Some(20.0), Some(85.0))]),
        };

        let result = check_rate_limits(&rate_limits, &args, false).unwrap();

        assert_eq!(result.exit_code, 1);
        assert!(
            result
                .stdout
                .starts_with("rate limit threshold reached: codex weekly limit 15% left <= 20%")
        );
    }

    #[test]
    fn check_with_mock_unavailable_rate_limits_exits_one() {
        let args = CheckArgs {
            remaining_percent: 10.0,
            window: WaitWindow::Both,
        };

        let result =
            check_rate_limits(&unavailable("chatgpt authentication required"), &args, true)
                .unwrap();
        let json: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();

        assert_eq!(result.exit_code, 1);
        assert_eq!(json["status"], "unavailable");
        assert_eq!(json["reason"], "chatgpt authentication required");
    }

    #[test]
    fn check_with_anonymous_snapshot_fixture_covers_wire_shape() {
        let snapshots: Vec<RateLimitSnapshot> =
            serde_json::from_str(include_str!("../tests/fixtures/rate_limit_snapshots.json"))
                .unwrap();
        let rate_limits = RateLimitsOutput {
            status: RateLimitStatus::Available,
            reason: None,
            limits: normalize_rate_limits(snapshots),
        };
        let args = CheckArgs {
            remaining_percent: 15.0,
            window: WaitWindow::FiveHour,
        };

        let result = check_rate_limits(&rate_limits, &args, true).unwrap();
        let json: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();

        assert_eq!(result.exit_code, 1);
        assert_eq!(json["status"], "threshold_reached");
        assert_eq!(json["thresholdRemainingPercent"], 15.0);
        assert_eq!(json["windows"].as_array().unwrap().len(), 1);
        assert_eq!(json["windows"][0]["scope"], "additional_1");
        assert_eq!(json["windows"][0]["window"], "5h");
        assert_eq!(json["windows"][0]["remainingPercent"], 10.0);
        assert!(!result.stdout.contains("example_additional"));
        assert!(!result.stdout.contains("Example additional limit"));
        assert!(!result.stdout.contains("zz_example_monthly"));
        assert!(!result.stdout.contains("email"));
        assert!(!result.stdout.contains("token"));

        let args = CheckArgs {
            remaining_percent: 75.0,
            window: WaitWindow::Monthly,
        };
        let result = check_rate_limits(&rate_limits, &args, true).unwrap();
        let json: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();

        assert_eq!(result.exit_code, 1);
        assert_eq!(json["windows"].as_array().unwrap().len(), 1);
        assert_eq!(json["windows"][0]["scope"], "additional_2");
        assert_eq!(json["windows"][0]["window"], "monthly");
        assert_eq!(json["windows"][0]["remainingPercent"], 73.0);
        assert!(!result.stdout.contains("zz_example_monthly"));
    }

    #[test]
    fn parses_wait_command_with_flattened_check_options() {
        let cli = Cli::try_parse_from([
            "cxst",
            "wait",
            "--remaining-percent",
            "15",
            "--window",
            "5h",
            "--interval",
            "1m",
        ])
        .unwrap();

        match cli.command {
            Some(Command::Wait(args)) => {
                assert_eq!(args.check.remaining_percent, 15.0);
                assert_eq!(args.check.window, WaitWindow::FiveHour);
                assert_eq!(args.interval, Duration::from_secs(60));
            }
            other => panic!("expected wait command, got {other:?}"),
        }
    }
}
