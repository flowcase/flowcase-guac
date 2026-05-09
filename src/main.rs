mod cli;

use clap::Parser;

fn main() {
    let _cli = cli::Cli::parse();
    println!("flowcase-guac v0");
}
