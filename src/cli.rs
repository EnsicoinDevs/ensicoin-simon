use clap::{App, Arg, SubCommand};

fn is_uri(v: String) -> Result<(), String> {
    match v.parse::<http::Uri>() {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("invalid URI: {}", e)),
    }
}

fn is_address(v: String) -> Result<(), String> {
    if v.len() != 66 {
        Err("invalid address: the address should be 66 characters".to_string())
    } else if v.starts_with("02") || v.starts_with("03") {
        match num_bigint::BigUint::parse_bytes(v.as_bytes(), 16) {
            Some(_) => Ok(()),
            None => Err("invalid address: non hexadecimal string".to_string()),
        }
    } else {
        Err("invalid address: the address should start with 02 or 03".to_string())
    }
}

pub fn build_cli() -> App<'static, 'static> {
    app_from_crate!()
        .arg(
            Arg::with_name("uri")
                .short("u")
                .long("uri")
                .help("gRPC server uri")
                .default_value("http://localhost:4225")
                .takes_value(true)
                .validator(is_uri),
        )
        .arg(
            Arg::with_name("address")
                .short("a")
                .long("address")
                .help("address of the coinbase")
                .takes_value(true)
                .required(true)
                .validator(is_address),
        )
        .subcommand(
            SubCommand::with_name("completions")
                .about("Generates completion scripts for your shell")
                .arg(
                    Arg::with_name("SHELL")
                        .required(true)
                        .possible_values(&["bash", "fish", "zsh"])
                        .help("The shell to generate the script for"),
                ),
        )
}
