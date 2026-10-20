mod data_structures;
mod helpers;

use data_structures::App;

fn main() -> std::io::Result<()> {
    let mut terminal = ratatui::init();
    let result = App::new().run(&mut terminal);
    ratatui::restore();
    result
}
