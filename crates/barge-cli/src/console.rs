//! Reading a secret line from standard input without echoing it, for
//! `machine pair`. `read_secret` makes the decision through the `Console` seam:
//! echo is turned off only when standard input is a terminal, and a piped key
//! is read as it always was. The termios calls live in `StdinConsole`, the
//! CLI's only terminal syscalls.

use std::io::{self, BufRead};

/// Whether standard input is a terminal, and how to switch its echo.
pub(crate) trait Console {
    fn is_terminal(&self) -> bool;
    fn set_echo(&self, on: bool) -> io::Result<()>;
}

/// Read one line. On a terminal, echo is off for the read and back on after it,
/// even when the read fails.
pub(crate) fn read_secret(input: &mut impl BufRead, console: &impl Console) -> io::Result<String> {
    let mut line = String::new();
    if !console.is_terminal() {
        input.read_line(&mut line)?;
        return Ok(line);
    }
    console.set_echo(false)?;
    let read = input.read_line(&mut line);
    let restored = console.set_echo(true);
    read?;
    restored?;
    // The typed key was not echoed, so the cursor is still on the prompt line.
    eprintln!();
    Ok(line)
}

/// The real terminal on standard input.
pub(crate) struct StdinConsole;

impl Console for StdinConsole {
    fn is_terminal(&self) -> bool {
        // SAFETY: isatty reads only the descriptor number it is given.
        unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
    }

    fn set_echo(&self, on: bool) -> io::Result<()> {
        // SAFETY: a zeroed termios is plain old data, and tcgetattr fills it
        // before anything reads it.
        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: `term` is a live, writable termios for the duration of the call.
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut term) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if on {
            term.c_lflag |= libc::ECHO;
        } else {
            term.c_lflag &= !libc::ECHO;
        }
        // SAFETY: `term` holds the settings tcgetattr just returned for this
        // descriptor, with only the echo bit changed.
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &term) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{read_secret, Console};
    use std::cell::RefCell;
    use std::io::{self, Cursor, Read};

    struct FakeConsole {
        terminal: bool,
        calls: RefCell<Vec<&'static str>>,
    }

    impl FakeConsole {
        fn new(terminal: bool) -> Self {
            Self {
                terminal,
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl Console for FakeConsole {
        fn is_terminal(&self) -> bool {
            self.terminal
        }

        fn set_echo(&self, on: bool) -> io::Result<()> {
            self.calls
                .borrow_mut()
                .push(if on { "echo on" } else { "echo off" });
            Ok(())
        }
    }

    /// A reader that fails on the first read.
    struct Broken;

    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("read failed"))
        }
    }

    impl io::BufRead for Broken {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            Err(io::Error::other("read failed"))
        }

        fn consume(&mut self, _: usize) {}
    }

    #[test]
    fn a_terminal_key_is_read_with_echo_off_and_then_restored() {
        let console = FakeConsole::new(true);
        let key = read_secret(&mut Cursor::new("abc123\n"), &console).unwrap();
        assert_eq!(key, "abc123\n");
        assert_eq!(*console.calls.borrow(), ["echo off", "echo on"]);
    }

    #[test]
    fn a_piped_key_never_touches_echo() {
        let console = FakeConsole::new(false);
        let key = read_secret(&mut Cursor::new("abc123\n"), &console).unwrap();
        assert_eq!(key, "abc123\n");
        assert!(console.calls.borrow().is_empty());
    }

    #[test]
    fn echo_is_restored_even_when_the_read_fails() {
        let console = FakeConsole::new(true);
        assert!(read_secret(&mut Broken, &console).is_err());
        assert_eq!(*console.calls.borrow(), ["echo off", "echo on"]);
    }
}
