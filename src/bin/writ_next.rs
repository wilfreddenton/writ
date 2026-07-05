//! Entry point for the Linebender-stack shell (gpui-replacement migration).
//! Thin wrapper; all logic lives in `writ::shell`.

fn main() -> anyhow::Result<()> {
    writ::shell::run()
}
