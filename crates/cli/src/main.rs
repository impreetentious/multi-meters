use multimeters_core::AppEngine;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("warn")
        .with_target(false)
        .init();

    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!(
            "Usage: multimeters [provider] [--force]\n\
             Read limits through MultiMeters' five-minute cache and print JSON."
        );
        return Ok(());
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("multimeters {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let force = args.iter().any(|a| a == "--force");
    args.retain(|a| a != "--force");
    if args.len() > 1 {
        anyhow::bail!("expected at most one provider ID");
    }
    let filter = args.first().cloned();

    let engine = AppEngine::new()?;
    engine.seed_if_needed().await;
    if let Some(id) = filter.as_deref() {
        if !engine.provider_ids().iter().any(|known| known == id) {
            eprintln!("unknown provider: {id}");
            std::process::exit(2);
        }
        engine
            .refresh_one(id, force)
            .await
            .map_err(anyhow::Error::msg)?;
    } else {
        engine.refresh_all(force).await;
    }
    let json = engine
        .limits_json(filter.as_deref())
        .await
        .expect("provider was validated before serialization");
    println!("{}", serde_json::to_string_pretty(&json)?);
    Ok(())
}
