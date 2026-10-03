fn main() {
    #[cfg(not(feature = "server"))]
    dioxus::launch(mobius_ui::App);

    #[cfg(feature = "server")]
    serve();
}

#[cfg(feature = "server")]
const HELP: &str = "\
Usage: mobius [COMMAND]

Commands:
  (none)  Start the server
  init    Write the config file

Options:
  -h, --help  Show this help text
";

#[cfg(feature = "server")]
fn serve() {
    use std::env;
    use std::net::{IpAddr, Ipv4Addr};
    use std::path::PathBuf;

    use dioxus::server::axum::Extension;

    let init = match env::args().nth(1).as_deref() {
        None => false,
        Some("init") => true,
        Some("--help" | "-h") => return print!("{HELP}"),
        Some(argument) => {
            eprintln!("mobius: unknown argument `{argument}`\n\n{HELP}");
            std::process::exit(2);
        }
    };

    if dioxus::cli_config::server_port().is_none() {
        // SAFETY: no other thread runs yet. `dioxus::serve` reads the port only from `PORT`.
        unsafe { env::set_var("PORT", "6363") };
    }

    let config_path = env::var_os("MOBIUS_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            env::home_dir()
                .unwrap_or_default()
                .join(".mobius/config.toml")
        });
    let path = env::var_os("PATH").unwrap_or_default();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| fail(error));
    if init {
        let (mut input, mut output) = (std::io::stdin().lock(), std::io::stdout());
        let init = mobius_engine::init::run(&mut input, &mut output, &config_path, &path);
        return runtime.block_on(init).unwrap_or_else(|error| fail(error));
    }
    let config = mobius_engine::config::load(&config_path).unwrap_or_else(|error| fail(error));

    let missing = mobius_engine::missing_commands(&config, &path);
    for program in &missing {
        eprintln!("mobius: `{program}` is not on PATH");
    }
    if !missing.is_empty() {
        std::process::exit(1);
    }

    if let Some(ip) = dioxus::cli_config::server_ip()
        && ip != IpAddr::V4(Ipv4Addr::LOCALHOST)
        && ip != IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    {
        fail(format!("IP: must be 127.0.0.1 or 0.0.0.0, not {ip}"));
    }

    let _runtime_guard = runtime.enter();
    let store = runtime
        .block_on(mobius_store::Store::open(&config.data_dir))
        .unwrap_or_else(|error| {
            fail(format!(
                "{}: {error}",
                config.data_dir.join("mobius.db").display()
            ))
        });
    let engine = runtime
        .block_on(mobius_engine::start(
            config,
            store.clone(),
            "https://api.github.com",
            "https://github.com",
            path,
            dioxus::cli_config::server_port().unwrap_or(6363),
        ))
        .unwrap_or_else(|error| fail(error));

    dioxus::serve(move || {
        let engine = engine.clone();
        let store = store.clone();
        async move {
            Ok(mobius_ui::router()
                .merge(mobius_engine::mcp::router(engine.clone()))
                .merge(mobius_engine::gh::router(engine.clone()))
                .layer(Extension(engine))
                .layer(Extension(store)))
        }
    });
}

#[cfg(feature = "server")]
fn fail(message: impl std::fmt::Display) -> ! {
    eprintln!("mobius: {message}");
    std::process::exit(1)
}
