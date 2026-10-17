fn main() {
    let code = railspotting_engine::cli::main_with(std::env::args().collect(), &mut std::io::stdout());
    std::process::exit(code);
}
