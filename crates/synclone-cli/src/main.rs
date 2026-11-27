use clap::Parser;

#[derive(Parser)]
#[command(name = "synclonectl", version, about = "Control Synclone from the command line")]
struct Cli {}

fn main() {
    let _ = Cli::parse();
    println!("{} {}", synclone_core::APP_NAME, synclone_core::VERSION);
}
