//! Entrypoint binary `oopbot`: dispatcher tipis menuju mode `host`, `rpc`, `catalog`,
//! `scan-models`, atau antarmuka CLI interaktif / single-prompt (`oopbot::interfaces::cli`).

use oopbot::common::BoxError;
use oopbot::config::builtin_catalog_json;
use oopbot::interfaces::cli;

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let args = match cli::parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("❌ {}", e);
            std::process::exit(2);
        }
    };
    cli::load_env();

    match args.mode.as_deref() {
        Some("host") => {
            let host_mode = args.host_mode.unwrap_or_else(|| "rpc".to_string());
            let port = args.host_port.unwrap_or(0);
            if port == 0 {
                eprintln!("❌ Port host tidak valid");
                std::process::exit(1);
            }
            oopbot::interfaces::host::run_host_mode(&host_mode, port).await
        }
        Some("rpc") => {
            oopbot::interfaces::rpc::run_rpc_mode(
                args.session,
                args.system_append,
                args.extensions,
            )
            .await
        }
        Some("catalog") => {
            println!("{}", serde_json::to_string(&builtin_catalog_json())?);
            Ok(())
        }
        Some("scan-models") => cli::run_scan_models().await,
        _ => cli::run_interactive_or_prompt(args).await,
    }
}
