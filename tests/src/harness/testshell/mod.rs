extern crate pty;
extern crate vte;

use std::os::unix::process::CommandExt;
use std::process::Command;
use std::io::Read;
use std::io::Write;
use std::time::Duration;
use std::sync::mpsc;
use std::thread;

pub struct TestShell {
    // fork is here for lifetime reasons; can't drop it until the pty is done
    #[allow(unused)] fork: pty::fork::Fork,
    pty: pty::fork::Master,
    output: mpsc::Receiver<String>,
    eof: mpsc::Receiver<()>,
}

// VTEData is to handle lines after the mess of vte terminal stuff.
// It keeps track of newlines and such
#[derive(Debug)]
struct VTEData {
    current_line_cursor: usize,
    pub current_line: String,
    pub scrollback: Vec<String>,
}

// VTEDataLen is used as a sorta cheap hash for comparing whether VTEData has changed in a
// meaningful way.
#[derive(PartialEq, Clone, Debug)]
struct VTEDataLen {
    pub current_line: usize,
    pub scrollback: usize,
}

impl VTEData {
    fn new() -> Self {
        VTEData {
            current_line_cursor: 0,
            current_line: String::new(),
            scrollback: Vec::new(),
        }
    }

    fn len(&self) -> VTEDataLen {
        VTEDataLen {
            current_line: self.current_line.len(),
            scrollback: self.scrollback.len(),
        }
    }
}

impl vte::Perform for VTEData {
    fn print(&mut self, c: char) {
        self.current_line.truncate(self.current_line_cursor);
        self.current_line_cursor += 1;
        self.current_line.push(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte as char {
            '\n' => {
                self.scrollback.push(self.current_line.clone());
                self.current_line.truncate(0);
            }
            '\r' => {
                self.current_line_cursor = 0;
            }
            '\x08' => {
                // backspace
                if self.current_line_cursor > 0 {
                    self.current_line_cursor -= 1;
                    self.current_line.pop();
                }
            }
            '\t' => {
                self.print('\t');
            }
            _ => {
                println!("[VTEData execute]: ignoring {}", byte);
            }
        }
    }

    fn hook(&mut self, _: &[i64], _: &[u8], _: bool) {
        // ignore
    }

    fn put(&mut self, _: u8) {
        // ignore
    }

    fn unhook(&mut self) {
        // ignore
    }

    fn osc_dispatch(&mut self, _: &[&[u8]]) {
        // ignore
    }

    fn csi_dispatch(&mut self, _: &[i64], _: &[u8], _: bool, _: char) {
        // ignore
    }

    fn esc_dispatch(&mut self, _: &[i64], _: &[u8], _: bool, _: u8) {
        // ignore
    }
}

impl TestShell {
    // TODO pattern instead of regex once that's stable
    // new creates a new testshell. It is assumed that the passed in command is for a posix-ish
    // shell. The shell should print output generally line-by-line and after executing a command,
    // it should print the PS1 variable.
    // This PS1 variable is used to determine when commands have executed, so no commands run in
    // this testshell may print the PS1 value.
    // Note: this command does fork off a child. There are dragons. Handle with care.
    pub fn new(mut cmd: Command, ps1: &str) -> Self {
        cmd.env("PS1", ps1);
        let fork = pty::fork::Fork::from_ptmx().unwrap();

        let mut pty = match fork {
            pty::fork::Fork::Child(_) => {
                let err = cmd.exec();
                panic!("exec failed: {}", err);
            }
            pty::fork::Fork::Parent(_, m) => {
                m.grantpt().unwrap();
                m.unlockpt().unwrap();
                m
            }
        };

        let (write_command_out, command_out) = mpsc::channel();
        let (write_eof_got, eof_got) = mpsc::channel();

        // To move into the thread
        let ps12 = ps1.to_owned();
        thread::spawn(move || {
            // vte stuff
            let mut data = VTEData::new();
            let mut statemachine = vte::Parser::new();
            // Keep a record of the last vte-length info we saw so we can detect meaningful
            // changes.
            let mut last_len = data.len();

            // Have we seen the starting PS1 yet?
            let mut last_prompt_scrollback_count = -1;
            // What's been output since the last PS1 + command happened?
            let mut current_command_output = Vec::new();
            loop {
                let mut buf: [u8; 4 * 1024] = [0; 4 * 1024];
                let nread = pty.read(&mut buf).unwrap();
                if nread == 0 {
                    write_eof_got.send(()).unwrap();
                    // EOF
                    return;
                }
                for byte in &buf[..nread] {
                    statemachine.advance(&mut data, *byte);
                    if last_len == data.len() {
                        // control character or whatever, we don't care
                        continue;
                    }

                    if data.current_line == ps12
                        && last_prompt_scrollback_count < data.scrollback.len() as i32
                    {
                        // Exactly equal to PS1 means that there's a new blank PS1 prompt
                        // Either we just started up, or a command just finished.
                        // not startup, sometihng finished
                        write_command_out
                            .send(current_command_output.join("\n"))
                            .unwrap();
                        current_command_output.truncate(0);
                        // mark that we've seen this prompt, don't handle it again even if there's
                        // backspacing
                        last_prompt_scrollback_count = data.scrollback.len() as i32;
                    } else if data.scrollback.len() > last_len.scrollback
                        && last_prompt_scrollback_count != -1
                    {
                        // this only happens if the last character was a newline since we're
                        // checking this every statemachine advance.
                        let last_line = data.scrollback.last().unwrap();
                        // skip PS1 starting things since we assume that's a command being entered,
                        // e.g....
                        //     PS1 $ ls
                        //     file1 file2
                        //     PS1 $
                        // We're avoiding the first line there
                        if !last_line.starts_with(&ps12) {
                            current_command_output.push(last_line.to_string());
                        }
                    }
                    last_len = data.len();
                }
            }
        });

        command_out
            .recv_timeout(Duration::from_secs(5))
            .expect("did not get initial prompt");

        TestShell {
            fork: fork,
            pty: pty,
            eof: eof_got,
            output: command_out,
        }
    }

    pub fn run(&mut self, cmd: &str) -> String {
        self.pty.write(format!("{}\n", cmd).as_bytes()).unwrap();
        self.output.recv_timeout(Duration::from_secs(100)).unwrap()
    }

    pub fn shutdown(&mut self) {
        self.pty.write("exit\n".as_bytes()).unwrap();
        self.eof.recv().unwrap();
    }
}

#[cfg(features = "testshell-dev")]
mod dev {
    use std::process::Command;
    use super::TestShell;
    #[test]
    fn testshell() {
        let mut cmd = Command::new("zsh");
        let mut ts = TestShell::new(cmd, "==> ");
        assert_eq!(ts.run("cd /tmp"), "");
        assert_eq!(ts.run("echo foo"), "foo");
        assert_eq!(ts.run(r#"echo -e "foo\nbar\nbaz" | tac"#), "baz\nbar\nfoo");
        ts.shutdown();
    }
}
