use clap::Parser as _;

fn main() {
    let cli = board::cli::Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("BOARD_LOG")
                // The daemon also says what it does (starts, adoptions, reconnects) in
                // watch.log; everything else speaks only of trouble.
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,board::daemon=info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("starting the async runtime");
    let code = runtime.block_on(board::cli::run(cli));
    // Exit without waiting for stray blocking tasks (a hook must not outlive its budget).
    std::process::exit(code);
}
