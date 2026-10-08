mod audio;
mod ltc;
mod ntp;
mod status;

mod ui;

fn main() -> eframe::Result<()> {
    ui::run()
}
