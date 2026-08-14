use mush_cli::config::Config;

fn main() {
    println!("{}", nixcfg::emit::<Config>("mush"));
}
