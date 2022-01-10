use std::{fs::File, io::Read, os::unix::prelude::FromRawFd};

use nix::{
    fcntl::OFlag,
    sys::termios::{
        self, ControlFlags, FlushArg, InputFlags, LocalFlags, OutputFlags, SpecialCharacterIndices,
    },
    sys::{stat::Mode, termios::BaudRate},
};

fn main() {
    let port = std::env::args().nth(1).unwrap();

    let fd = nix::fcntl::open(
        port.as_str(),
        OFlag::O_NOCTTY | OFlag::O_RDWR,
        Mode::empty(),
    )
    .unwrap();

    let old_tio = termios::tcgetattr(fd).unwrap();

    let mut new_tio = old_tio.clone();

    new_tio.control_flags = ControlFlags::CS8 | ControlFlags::CLOCAL | ControlFlags::CREAD;
    new_tio.input_flags = InputFlags::IGNPAR | InputFlags::ICRNL;
    new_tio.output_flags = OutputFlags::empty();
    new_tio.local_flags = LocalFlags::empty();
    new_tio.control_chars = [0; 32];

    new_tio.control_chars[SpecialCharacterIndices::VMIN as usize] = 1; /* blocking read until 1 character arrives */

    termios::cfmakeraw(&mut new_tio);
    termios::cfsetospeed(&mut new_tio, BaudRate::B115200).unwrap();

    termios::tcflush(fd, FlushArg::TCIFLUSH).unwrap();
    termios::tcsetattr(fd, termios::SetArg::TCSANOW, &new_tio).unwrap();

    let mut port = unsafe { File::from_raw_fd(fd) };

    loop {
        let mut buf = [0; 256];

        let n = port.read(&mut buf[..]).unwrap();

        print!("{}", String::from_utf8_lossy(&buf[..n]));
    }

    termios::tcsetattr(fd, termios::SetArg::TCSANOW, &old_tio).unwrap();
}
