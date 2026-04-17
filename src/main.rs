mod api;
mod app;
mod model;
mod oauth;
mod storage;

fn main() -> anyhow::Result<()> {
    app::run()
}
