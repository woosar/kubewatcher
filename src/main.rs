mod data_structures;
mod helpers;

/// Terminal setup and teardown need a real tty, so `main` is left out of test builds.
#[cfg(not(test))]
fn main() -> std::io::Result<()> {
    let mut terminal = ratatui::init();
    let result = data_structures::App::new().run(&mut terminal);
    ratatui::restore();
    result
}
