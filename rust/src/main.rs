#![windows_subsystem = "windows"]

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(metabrain::app::main_with(args, false));
}
