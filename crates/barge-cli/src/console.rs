//! Reading a secret line from standard input without echoing it, for
//! `machine pair`. `read_secret` makes the decision through the `Console` seam:
//! on a terminal it turns echo off for the read, but only when echo was on, and
//! puts back exactly the state it found. A piped key is read as it always was.
//! The termios calls live in `StdinConsole`, the CLI's only terminal syscalls.

use std::io::{self, BufRead};

/// Whether standard input is a terminal, and its echo state.
pub(crate) trait Console {
    fn is_terminal(&self) -> bool;
    fn echo(&self) -> io::Result<bool>;
    fn set_echo(&self, on: bool) -> io::Result<()>;
}

/// Read one line. On a terminal, echo is turned off for the read only if it was
/// on, and it is restored to what it was even when the read fails. A terminal
/// that already had echo off is left that way.
pub(crate) fn read_secret(input: &mut impl BufRead, console: &impl Console) -> io::Result<String> {
    let mut line = String::new();
    if !console.is_terminal() {
        input.read_line(&mut line)?;
        return Ok(line);
    }
    let was_echoing = console.echo()?;
    if was_echoing {
        console.set_echo(false)?;
    }
    let read = input.read_line(&mut line);
    let restored = if was_echoing {
        console.set_echo(true)
    } else {
        Ok(())
    };
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

    fn echo(&self) -> io::Result<bool> {
        Ok((attributes()?.c_lflag & libc::ECHO) != 0)
    }

    fn set_echo(&self, on: bool) -> io::Result<()> {
        // Only the echo bit changes; every other terminal setting is kept.
        let mut term = attributes()?;
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

/// The current terminal settings of standard input.
fn attributes() -> io::Result<libc::termios> {
    // SAFETY: a zeroed termios is plain old data, and tcgetattr fills it before
    // anything reads it.
    let mut term: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: `term` is a live, writable termios for the duration of the call.
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut term) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(term)
}

#[cfg(test)]
mod tests {
    use super::{read_secret, Console};
    use std::cell::{Cell, RefCell};
    use std::io::{self, Cursor, Read};

    struct FakeConsole {
        terminal: bool,
        echoing: Cell<bool>,
        calls: RefCell<Vec<&'static str>>,
    }

    impl FakeConsole {
        fn new(terminal: bool, echoing: bool) -> Self {
            Self {
                terminal,
                echoing: Cell::new(echoing),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl Console for FakeConsole {
        fn is_terminal(&self) -> bool {
            self.terminal
        }

        fn echo(&self) -> io::Result<bool> {
            Ok(self.echoing.get())
        }

        fn set_echo(&self, on: bool) -> io::Result<()> {
            self.echoing.set(on);
            self.calls
                .borrow_mut()
                .push(if on { "echo on" } else { "echo off" });
            Ok(())
        }
    }

    /// A reader that fails on every read.
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
        let console = FakeConsole::new(true, true);
        let key = read_secret(&mut Cursor::new("abc123\n"), &console).unwrap();
        assert_eq!(key, "abc123\n");
        assert_eq!(*console.calls.borrow(), ["echo off", "echo on"]);
        assert!(console.echoing.get());
    }

    #[test]
    fn a_terminal_that_already_has_echo_off_keeps_it_off() {
        let console = FakeConsole::new(true, false);
        let key = read_secret(&mut Cursor::new("abc123\n"), &console).unwrap();
        assert_eq!(key, "abc123\n");
        assert!(console.calls.borrow().is_empty());
        assert!(!console.echoing.get());
    }

    #[test]
    fn a_piped_key_never_touches_echo() {
        let console = FakeConsole::new(false, true);
        let key = read_secret(&mut Cursor::new("abc123\n"), &console).unwrap();
        assert_eq!(key, "abc123\n");
        assert!(console.calls.borrow().is_empty());
        assert!(console.echoing.get());
    }

    #[test]
    fn echo_is_restored_even_when_the_read_fails() {
        let console = FakeConsole::new(true, true);
        assert!(read_secret(&mut Broken, &console).is_err());
        assert_eq!(*console.calls.borrow(), ["echo off", "echo on"]);
        assert!(console.echoing.get());
    }

    #[test]
    fn echo_already_off_stays_off_when_the_read_fails() {
        let console = FakeConsole::new(true, false);
        assert!(read_secret(&mut Broken, &console).is_err());
        assert!(console.calls.borrow().is_empty());
        assert!(!console.echoing.get());
    }
}
