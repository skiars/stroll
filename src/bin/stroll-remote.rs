fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    match (args.next().as_deref(), args.next(), args.next()) {
        (None, None, None) => stroll::remote::serve(),
        (Some("--lsp"), Some(options), None) => stroll::remote::serve_lsp(&options),
        _ => anyhow::bail!("Invalid remote helper arguments"),
    }
}
