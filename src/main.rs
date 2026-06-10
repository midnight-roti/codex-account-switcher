mod api;
mod app;
mod model;
mod oauth;
mod storage;
mod usage;

fn main() -> anyhow::Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    if args.get(1).map(String::as_str) == Some("usage") {
        return usage::run(&args[2..]);
    }
    app::run()
}
