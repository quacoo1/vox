//! The fake add-on helper for the vox-addons integration tests (`vox_addons::fake`). Never packaged.

fn main() {
    let args = std::env::args().skip(1).collect();
    std::process::exit(vox_addons::fake::run(args, std::io::stdin(), std::io::stdout(), std::io::stderr()));
}
