use std::collections::HashMap;

use crate::debugger_command::DebuggerCommand;
use crate::dwarf_data::{DwarfData, Error as DwarfError};
use crate::inferior::{Inferior, Status};
use object::SymbolKind::Null;
use rustyline::error::ReadlineError;
use rustyline::history::FileHistory;
use rustyline::Editor;

pub struct Debugger {
    target: String,
    history_path: String,
    readline: Editor<(), FileHistory>,
    inferior: Option<Inferior>,
    debug_data: DwarfData,
    break_points: HashMap<usize, Breakpoint>,
}

#[derive(Clone)]
pub struct Breakpoint {
    addr: usize,
    pub orig_byte: u8,
}

impl Debugger {
    /// Initializes the debugger.
    pub fn new(target: &str) -> Debugger {
        let debug_data = match DwarfData::from_file(target) {
            Ok(val) => val,
            Err(DwarfError::ErrorOpeningFile) => {
                println!("Could not open file {}", target);
                std::process::exit(1);
            }
            Err(DwarfError::DwarfFormatError(err)) => {
                println!("Could not debugging symbols from {}: {:?}", target, err);
                std::process::exit(1);
            }
        };

        debug_data.print();

        let history_path = format!("{}/.deet_history", std::env::var("HOME").unwrap());
        let mut readline = Editor::<(), FileHistory>::new().expect("Create Editor fail");
        // Attempt to load history from ~/.deet_history if it exists
        let _ = readline.load_history(&history_path);

        Debugger {
            target: target.to_string(),
            history_path,
            readline,
            inferior: None,
            debug_data,
            break_points: HashMap::new(),
        }
    }

    pub fn run(&mut self) {
        loop {
            match self.get_next_command() {
                DebuggerCommand::Run(args) => {
                    // kill the existed inferior
                    if let Some(mut old_inferior) = self.inferior.take() {
                        let pid = old_inferior.pid().as_raw();
                        old_inferior.kill();
                        old_inferior
                            .wait(None)
                            .expect("fail to recycle running inferior");
                        println!("Killing running inferior (pid {})", pid);
                    }
                    if let Some(inferior) =
                        Inferior::new(&self.target, &args, &mut self.break_points)
                    {
                        // Create the inferior
                        self.inferior = Some(inferior);
                        // restart subprocess
                        match self.inferior.as_mut().unwrap().cont(&self.break_points) {
                            Ok(status) => match status {
                                Status::Exited(num) => {
                                    // clean rest Child handler, avoid to call 'kill()' in Child
                                    // when it exits
                                    let _ = self.inferior.take();
                                    println!("Child exited (status {})", num);
                                }
                                Status::Stopped(sig, pos) => {
                                    println!("Child stopped (signal {})", sig.as_str());
                                    if let Some(line) = self.debug_data.get_line_from_addr(pos) {
                                        println!("Stopped at {}:{}", line.file, line.number);
                                    } else {
                                        // when stopped by ctrl+c, pos is the address in lib istread
                                        // of subprocess. In this case, line info can't be parse
                                        // from pos
                                        println!("Stopped at {:#x} (no debug line info)", pos);
                                    }
                                }
                                Status::Signaled(_) => println!("Child exited due to signal"),
                            },
                            Err(e) => {
                                println!("fail to restart subprocess: {}", e.desc());
                            }
                        }
                    } else {
                        println!("fail to start subprocess");
                    }
                }
                DebuggerCommand::Quit => {
                    if let Some(mut old_inferior) = self.inferior.take() {
                        let pid = old_inferior.pid().as_raw();
                        old_inferior.kill();
                        println!("Killing running inferior (pid {})", pid);
                    }
                    return;
                }
                DebuggerCommand::Continue => {
                    if let Some(inferior) = self.inferior.as_mut() {
                        // restart subprocess
                        match inferior.cont(&self.break_points) {
                            Ok(status) => match status {
                                Status::Exited(num) => {
                                    // clean rest Child handler, avoid to call 'kill()' in Child
                                    // when it exits
                                    let _ = self.inferior.take();
                                    println!("Child exited (status {})", num);
                                }
                                Status::Stopped(sig, pos) => {
                                    println!("Child stopped (signal {})", sig.as_str());
                                    if let Some(line) = self.debug_data.get_line_from_addr(pos) {
                                        println!("Stopped at {}:{}", line.file, line.number);
                                    } else {
                                        // when stopped by ctrl+c, pos is the address in lib istread
                                        // of subprocess. In this case, line info can't be parse
                                        // from pos
                                        println!("Stopped at {:#x} (no debug line info)", pos);
                                    }
                                }
                                Status::Signaled(_) => println!("Child exited due to signal"),
                            },
                            Err(e) => {
                                println!("fail to contunue the subprocess: {}", e.desc());
                            }
                        }
                    } else {
                        println!("There's no subprocess debugged");
                    }
                }
                DebuggerCommand::Backtrace => {
                    if let Some(inferior) = self.inferior.as_mut() {
                        if let Err(e) = inferior.print_backtrace(&self.debug_data) {
                            println!("fail to print backtrace: {}", e.desc());
                        }
                    } else {
                        println!("There's no subprocess debugged");
                    }
                }
                DebuggerCommand::Break(arg) => {
                    let ptr = parse_address(arg.as_str(), &self.debug_data)
                        .expect("fail to parse the address");
                    let mut break_point = Breakpoint {
                        addr: ptr,
                        orig_byte: 0,
                    };
                    // if subprocess has loaded, directly set break point in memory.
                    if let Some(inferior) = &mut self.inferior {
                        let orig_byte = inferior
                            .write_byte(ptr, 0xcc)
                            .expect("fail to write break points in memory");
                        break_point.orig_byte = orig_byte;
                    }
                    self.break_points.insert(ptr, break_point);
                    println!(
                        "Set breakpoint {} at {:#x}",
                        self.break_points.len() - 1,
                        ptr
                    );
                }
            }
        }
    }

    /// This function prompts the user to enter a command, and continues re-prompting until the user
    /// enters a valid command. It uses DebuggerCommand::from_tokens to do the command parsing.
    ///
    /// You don't need to read, understand, or modify this function.
    fn get_next_command(&mut self) -> DebuggerCommand {
        loop {
            // Print prompt and get next line of user input
            match self.readline.readline("(deet) ") {
                Err(ReadlineError::Interrupted) => {
                    // User pressed ctrl+c. We're going to ignore it
                    println!("Type \"quit\" to exit");
                }
                Err(ReadlineError::Eof) => {
                    // User pressed ctrl+d, which is the equivalent of "quit" for our purposes
                    return DebuggerCommand::Quit;
                }
                Err(err) => {
                    panic!("Unexpected I/O error: {:?}", err);
                }
                Ok(line) => {
                    if line.trim().len() == 0 {
                        continue;
                    }
                    let _ = self.readline.add_history_entry(line.as_str());
                    if let Err(err) = self.readline.save_history(&self.history_path) {
                        println!(
                            "Warning: failed to save history file at {}: {}",
                            self.history_path, err
                        );
                    }
                    let tokens: Vec<&str> = line.split_whitespace().collect();
                    if let Some(cmd) = DebuggerCommand::from_tokens(&tokens) {
                        return cmd;
                    } else {
                        println!("Unrecognized command.");
                    }
                }
            }
        }
    }
}
/// parse a usize from a hexadecimal string, or parse line number or function name from addr.
fn parse_address(addr: &str, data: &DwarfData) -> Option<usize> {
    // match address
    if addr.chars().nth(0).map_or(false, |c| c == '*') {
        let addr = &addr[1..];
        let addr_without_0x = if addr.to_lowercase().starts_with("0x") {
            &addr[2..]
        } else {
            &addr
        };
        return usize::from_str_radix(addr_without_0x, 16).ok();
    }
    // match line number
    if let Ok(line) = addr.parse::<usize>() {
        return data.get_addr_for_line(None, line);
    }
    // match function name
    data.get_addr_for_function(None, addr)
}
