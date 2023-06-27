use anyhow::{ensure, Result};
use std::ffi::OsStr;
use std::process::{Command, ExitStatus, Output};

pub(crate) trait CommandExt {
    fn succeed(&mut self) -> Result<()>;
    fn succeed_output(&mut self) -> Result<Output>;
    fn to_string(&self) -> String;
}

impl CommandExt for Command {
    fn succeed(&mut self) -> Result<()> {
        let status = self.status()?;
        check(self, status)
    }

    fn succeed_output(&mut self) -> Result<Output> {
        let output = self.output()?;
        check(self, output.status)?;
        Ok(output)
    }

    fn to_string(&self) -> String {
        shell_words::join(
            std::iter::once(self.get_program())
                .chain(self.get_args())
                .map(OsStr::to_string_lossy),
        )
    }
}

fn check(command: &Command, status: ExitStatus) -> Result<()> {
    ensure!(
        status.success(),
        "`{}` failed with {}",
        command.to_string(),
        status
    );
    Ok(())
}
