fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(g2h::cli::main_with(&argv));
}
