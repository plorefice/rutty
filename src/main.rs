//! Rutty is a terminal emulator and serial console that aims to supports the most common network
//! and file transfer protocols in a single command-line tool.

#![warn(missing_docs)]

use anyhow::Result;
use tokio::runtime::Builder;

mod cli;
pub mod serial;
pub mod ssh;
pub mod termios;
pub mod tty;

fn main() -> Result<()> {
    let runtime = Builder::new_current_thread()
        .thread_name("rutty")
        .enable_all()
        .build()?;

    runtime.block_on(async { cli::run().await })?;

    // Normally, this wouldn't be necessary. The problem here is that tokio::io::Stdin spawns
    // a new thread with a blocking read operation inside each time a read operation is issued,
    // causing the runtime thread to hang until the user presses enter (or EOF is reached).
    //
    // When using it to run an interactive shell however, an early termination of the shell, due
    // for example to the remote host closing the connection on its end, causes the program to hang
    // until the next enter keypress, which is kinda ugly, UX-wise.
    //
    // To avoid this, we drop the runtime in background, which immediately kills all pending tasks.
    // It shouldn't be an issue since all the other objects have already been dropped.
    runtime.shutdown_background();

    Ok(())
}
