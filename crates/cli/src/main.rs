//! Command line for Lighthouse.

use clap::Parser;

/// Command line for Lighthouse.
#[derive(Parser)]
#[command(name = "lht", version, about)]
struct Cli {}

fn main() {
    Cli::parse();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn command_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
