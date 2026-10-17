use std::io::Write;

use clap_complete::aot::{Bash, Elvish, Fish, Generator, Zsh};
use clap_complete_nushell::Nushell;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    Nushell,
    Elvish,
}

pub fn write(shell: Shell, command: clap::Command, out: &mut dyn Write) -> std::io::Result<()> {
    let mut command = match shell {
        Shell::Nushell => optional_values_as_switches(command),
        _ => command,
    };
    command.set_bin_name(command.get_name().to_owned());
    command.build();
    let generator: &dyn Generator = match shell {
        Shell::Bash => &Bash,
        Shell::Zsh => &Zsh,
        Shell::Fish => &Fish,
        Shell::Nushell => &Nushell,
        Shell::Elvish => &Elvish,
    };
    generator.try_generate(&command, out)
}

// nushell externs can't express an optional flag value, and a typed flag rejects a bare --resume
fn optional_values_as_switches(command: clap::Command) -> clap::Command {
    command
        .mut_args(|arg| match arg.get_num_args() {
            Some(range) if range.min_values() == 0 && range.takes_values() => arg.num_args(0),
            _ => arg,
        })
        .mut_subcommands(optional_values_as_switches)
}
