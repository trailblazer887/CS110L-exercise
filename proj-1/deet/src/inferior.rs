use nix::sys::ptrace;
use nix::sys::signal;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use std::collections::HashMap;
use std::mem::size_of;
use std::os::unix::process::CommandExt;
use std::process::Child;
use std::process::Command;

use crate::debugger::Breakpoint;
use crate::dwarf_data::DwarfData;

pub enum Status {
    /// Indicates inferior stopped. Contains the signal that stopped the process, as well as the
    /// current instruction pointer that it is stopped at.
    Stopped(signal::Signal, usize),

    /// Indicates inferior exited normally. Contains the exit status code.
    Exited(i32),

    /// Indicates the inferior exited due to a signal. Contains the signal that killed the
    /// process.
    Signaled(signal::Signal),
}

/// This function calls ptrace with PTRACE_TRACEME to enable debugging on a process. You should use
/// pre_exec with Command to call this in the child process.
fn child_traceme() -> Result<(), std::io::Error> {
    ptrace::traceme().or(Err(std::io::Error::new(
        std::io::ErrorKind::Other,
        "ptrace TRACEME failed",
    )))
}

pub struct Inferior {
    child: Child,
}

impl Inferior {
    /// Attempts to start a new inferior process. Returns Some(Inferior) if successful, or None if
    /// an error is encountered.
    pub fn new(
        target: &str,
        args: &Vec<String>,
        break_points: &mut HashMap<usize, Breakpoint>,
    ) -> Option<Inferior> {
        unsafe {
            let child = Command::new(target)
                .args(args)
                .pre_exec(child_traceme)
                .spawn()
                .ok()?;
            let mut inferior = Inferior { child };
            // ensure child was paused by SIGTRAP
            let status = inferior.wait(None).ok()?;
            match status {
                Status::Stopped(_, _) => {
                    // write in break points
                    for (addr, break_point) in break_points {
                        let orig_byte = inferior
                            .write_byte(*addr, 0xcc)
                            .expect("fail to write break points in memory");
                        break_point.orig_byte = orig_byte;
                    }
                    Some(inferior)
                }
                _ => None,
            }
        }
    }
    /// restart the inferior with PTRACE_CONT until status changes.
    /// if successful, return latest status
    pub fn cont(
        &mut self,
        break_points: &HashMap<usize, Breakpoint>,
    ) -> Result<Status, nix::Error> {
        let current_addr = ptrace::getregs(self.pid())?.rip as usize;
        if break_points.contains_key(&current_addr) {
            let _ = ptrace::step(self.pid(), None)?;
            let status = self.wait(None)?;
            // if subprocess exits, 'write_byte' will fail.
            match status {
                Status::Exited(_) | Status::Signaled(_) => {
                    return Ok(status);
                }
                _ => {}
            }
            self.write_byte(current_addr, 0xcc)?;
        }

        let _ = ptrace::cont(self.pid(), None)?;
        let res = self.wait(None)?;
        // if subprocess exits, 'getregs' will fail.
        match res {
            Status::Exited(_) | Status::Signaled(_) => {
                return Ok(res);
            }
            _ => {}
        }

        let mut regs = ptrace::getregs(self.pid())?;
        let bp_addr = regs.rip as usize - 1;
        if let Some(break_point) = break_points.get(&bp_addr) {
            self.write_byte(bp_addr, break_point.orig_byte)?;
            regs.rip -= 1;
            ptrace::setregs(self.pid(), regs)?;
        }
        Ok(res)
    }

    pub fn kill(&mut self) {
        self.child.kill().expect("fail to kill the subprocess");
    }

    pub fn print_backtrace(&self, debug_data: &DwarfData) -> Result<(), nix::Error> {
        let regs = ptrace::getregs(self.pid())?;
        let mut instruction_ptr = regs.rip;
        let mut base_ptr = regs.rbp;
        while true {
            let line = debug_data
                .get_line_from_addr(instruction_ptr as usize)
                .expect("fail to get line from address");
            let func = debug_data
                .get_function_from_addr(instruction_ptr as usize)
                .expect("fail to get function from address");
            println!("{} ({}:{})", func, line.file, line.number);
            if func == "main" {
                break;
            }
            instruction_ptr =
                ptrace::read(self.pid(), (base_ptr + 8) as ptrace::AddressType)? as u64;
            base_ptr = ptrace::read(self.pid(), base_ptr as ptrace::AddressType)? as u64;
        }
        Ok(())
    }

    pub fn write_byte(&mut self, addr: usize, val: u8) -> Result<u8, nix::Error> {
        let aligned_addr = align_addr_to_word(addr);
        let byte_offset = addr - aligned_addr;
        let word = ptrace::read(self.pid(), aligned_addr as ptrace::AddressType)? as u64;
        let orig_byte = (word >> 8 * byte_offset) & 0xff;
        let masked_word = word & !(0xff << 8 * byte_offset);
        let updated_word = masked_word | ((val as u64) << 8 * byte_offset);
        unsafe {
            ptrace::write(
                self.pid(),
                aligned_addr as ptrace::AddressType,
                updated_word as *mut std::ffi::c_void,
            )?;
        }
        Ok(orig_byte as u8)
    }

    /// Returns the pid of this inferior.
    pub fn pid(&self) -> Pid {
        nix::unistd::Pid::from_raw(self.child.id() as i32)
    }

    /// Calls waitpid on this inferior and returns a Status to indicate the state of the process
    /// after the waitpid call.
    pub fn wait(&self, options: Option<WaitPidFlag>) -> Result<Status, nix::Error> {
        Ok(match waitpid(self.pid(), options)? {
            WaitStatus::Exited(_pid, exit_code) => Status::Exited(exit_code),
            WaitStatus::Signaled(_pid, signal, _core_dumped) => Status::Signaled(signal),
            WaitStatus::Stopped(_pid, signal) => {
                let regs = ptrace::getregs(self.pid())?;
                Status::Stopped(signal, regs.rip as usize)
            }
            other => panic!("waitpid returned unexpected status: {:?}", other),
        })
    }
}
fn align_addr_to_word(addr: usize) -> usize {
    addr & (-(size_of::<usize>() as isize) as usize)
}
