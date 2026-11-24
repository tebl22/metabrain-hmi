// Console companion (metabrain-hook.exe): invoked by Claude Code with the
// event name as the first argument. `console = true` routes straight to the
// hook runner inside main_with.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(metabrain::app::main_with(args, true));
}
