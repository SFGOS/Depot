use super::*;
use crate::commands::update::check::run_check_command;

pub(super) fn run_check(args: crate::cli::CheckArgs) -> Result<()> {
    let dir = args.dir;
    run_check_command(&dir)
}
