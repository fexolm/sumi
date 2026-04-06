pub(crate) mod breakpoints;
mod rsp;

use breakpoints::BreakpointManager;
use rsp::{decode_hex, encode_hex, parse_hex_num};

use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::Arc;
use vm_memory::GuestMemoryMmap;

// --- Channel types between GDB stub thread and vCPU thread ---

pub enum DebugCommand {
    Continue,
    SingleStep,
    ReadRegisters,
    ReadMemory { addr: u64, len: usize },
    WriteMemory { addr: u64, data: Vec<u8> },
    InsertSwBreakpoint(u64),
    RemoveSwBreakpoint(u64),
    Detach,
    Kill,
}

pub enum DebugEvent {
    Stopped(StopReason),
    Registers(RegisterFile),
    Memory(Vec<u8>),
    Ok,
    Error(String),
}

#[derive(Debug, Clone)]
pub enum StopReason {
    /// Software breakpoint (int3)
    Breakpoint,
    /// Single-step completed
    SingleStep,
    /// Signal (SIGTRAP=5, SIGINT=2, SIGSEGV=11, etc.)
    Signal(u8),
    /// VM exited normally
    Exited,
}

/// x86-64 register file in the order GDB expects for the 'g' command.
/// 16 GP regs + RIP + EFLAGS + 6 segment regs = 24 × 8 bytes +
/// EFLAGS is 4 bytes but we pad to 8 for simplicity.
/// We send GP regs, RIP, EFLAGS, segment regs. FP/SSE registers are sent as zeros.
#[derive(Clone, Default)]
pub struct RegisterFile {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub rsp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub eflags: u32,
    pub cs: u32,
    pub ss: u32,
    pub ds: u32,
    pub es: u32,
    pub fs: u32,
    pub gs: u32,
}

impl RegisterFile {
    /// Serialize to GDB 'g' response format.
    /// GDB x86-64 expects: rax,rbx,rcx,rdx,rsi,rdi,rbp,rsp,r8-r15,rip,eflags(4),
    /// cs(4),ss(4),ds(4),es(4),fs(4),gs(4), then st0-st7(10 each), fctrl..fop(4 each),
    /// xmm0-xmm15(16 each), mxcsr(4).
    pub fn to_gdb_hex(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(624 * 2); // rough upper bound

        // 16 GP registers (8 bytes each, LE)
        for &val in &[
            self.rax, self.rbx, self.rcx, self.rdx, self.rsi, self.rdi, self.rbp, self.rsp,
            self.r8, self.r9, self.r10, self.r11, self.r12, self.r13, self.r14, self.r15,
        ] {
            buf.extend_from_slice(&encode_hex(&val.to_le_bytes()));
        }

        // RIP (8 bytes)
        buf.extend_from_slice(&encode_hex(&self.rip.to_le_bytes()));

        // EFLAGS (4 bytes)
        buf.extend_from_slice(&encode_hex(&self.eflags.to_le_bytes()));

        // Segment registers (4 bytes each)
        for &val in &[self.cs, self.ss, self.ds, self.es, self.fs, self.gs] {
            buf.extend_from_slice(&encode_hex(&val.to_le_bytes()));
        }

        // ST0-ST7: 8 × 10 bytes = 80 bytes of zeros
        for _ in 0..80 {
            buf.extend_from_slice(b"00");
        }

        // FPU control registers: fctrl, fstat, ftag, fiseg, fioff, foseg, fooff, fop
        // 8 × 4 bytes = 32 bytes of zeros
        for _ in 0..32 {
            buf.extend_from_slice(b"00");
        }

        // XMM0-XMM15: 16 × 16 bytes = 256 bytes of zeros
        for _ in 0..256 {
            buf.extend_from_slice(b"00");
        }

        // MXCSR: 4 bytes of zeros
        for _ in 0..4 {
            buf.extend_from_slice(b"00");
        }

        buf
    }
}

/// Sender half for the vCPU side.
pub struct VCpuDebugReceiver {
    pub cmd_rx: Receiver<DebugCommand>,
    pub event_tx: Sender<DebugEvent>,
}

/// Create the channel pair for GDB stub <-> vCPU communication.
pub fn create_debug_channels() -> (Sender<DebugCommand>, Receiver<DebugEvent>, VCpuDebugReceiver) {
    let (cmd_tx, cmd_rx) = channel();
    let (event_tx, event_rx) = channel();
    (cmd_tx, event_rx, VCpuDebugReceiver { cmd_rx, event_tx })
}

/// The GDB server. Runs in its own thread.
pub struct GdbServer {
    cmd_tx: Sender<DebugCommand>,
    event_rx: Receiver<DebugEvent>,
    mem: Arc<GuestMemoryMmap<()>>,
    breakpoints: BreakpointManager,
}

impl GdbServer {
    pub fn new(
        cmd_tx: Sender<DebugCommand>,
        event_rx: Receiver<DebugEvent>,
        mem: Arc<GuestMemoryMmap<()>>,
    ) -> Self {
        Self {
            cmd_tx,
            event_rx,
            mem,
            breakpoints: BreakpointManager::new(),
        }
    }

    /// Listen on the given port, accept one connection, and run the GDB protocol loop.
    pub fn run(mut self, port: u16) {
        let listener = match TcpListener::bind(format!("0.0.0.0:{}", port)) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[gdb] failed to bind to port {}: {}", port, e);
                return;
            }
        };
        eprintln!("[gdb] listening on port {}, waiting for GDB to connect...", port);

        let (stream, addr) = match listener.accept() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[gdb] accept failed: {}", port);
                let _ = e;
                return;
            }
        };
        eprintln!("[gdb] GDB connected from {}", addr);

        // Disable Nagle for responsive packet exchange
        let _ = stream.set_nodelay(true);

        if let Err(e) = self.protocol_loop(stream) {
            eprintln!("[gdb] protocol error: {}", e);
        }
        eprintln!("[gdb] session ended");
    }

    fn protocol_loop(&mut self, mut stream: TcpStream) -> std::io::Result<()> {
        // GDB initially stops the target — we start paused, so report stop.
        // The vCPU thread starts paused and waits for Continue.

        loop {
            let pkt = match rsp::read_packet(&mut stream)? {
                Some(data) => data,
                None => {
                    // Ctrl+C interrupt — not applicable when already stopped, but send SIGTRAP
                    rsp::send_stop_signal(&mut stream, 2)?; // SIGINT
                    continue;
                }
            };

            if pkt.is_empty() {
                continue;
            }

            match pkt[0] {
                b'?' => {
                    // Stop reason query — we start stopped
                    rsp::send_stop_trap(&mut stream)?;
                }
                b'g' => {
                    // Read all registers
                    self.cmd_tx.send(DebugCommand::ReadRegisters).ok();
                    match self.event_rx.recv() {
                        Ok(DebugEvent::Registers(regs)) => {
                            let hex = regs.to_gdb_hex();
                            rsp::send_packet(&mut stream, &hex)?;
                        }
                        _ => rsp::send_error(&mut stream, 1)?,
                    }
                }
                b'G' => {
                    // Write all registers — we don't support this yet
                    rsp::send_ok(&mut stream)?;
                }
                b'p' => {
                    // Read single register
                    self.handle_read_single_register(&pkt[1..], &mut stream)?;
                }
                b'P' => {
                    // Write single register — not supported yet
                    rsp::send_ok(&mut stream)?;
                }
                b'm' => {
                    // Read memory: m addr,length
                    self.handle_read_memory(&pkt[1..], &mut stream)?;
                }
                b'M' => {
                    // Write memory: M addr,length:XX...
                    self.handle_write_memory(&pkt[1..], &mut stream)?;
                }
                b'X' => {
                    // Binary write memory
                    rsp::send_empty(&mut stream)?;
                }
                b'c' => {
                    // Continue
                    self.cmd_tx.send(DebugCommand::Continue).ok();
                    self.wait_for_stop(&mut stream)?;
                }
                b's' => {
                    // Single step
                    self.cmd_tx.send(DebugCommand::SingleStep).ok();
                    self.wait_for_stop(&mut stream)?;
                }
                b'Z' => {
                    // Insert breakpoint: Z type,addr,kind
                    self.handle_insert_breakpoint(&pkt[1..], &mut stream)?;
                }
                b'z' => {
                    // Remove breakpoint: z type,addr,kind
                    self.handle_remove_breakpoint(&pkt[1..], &mut stream)?;
                }
                b'H' => {
                    // Thread operations — always OK (single thread)
                    rsp::send_ok(&mut stream)?;
                }
                b'D' => {
                    // Detach
                    self.cmd_tx.send(DebugCommand::Detach).ok();
                    rsp::send_ok(&mut stream)?;
                    return Ok(());
                }
                b'k' => {
                    // Kill
                    self.cmd_tx.send(DebugCommand::Kill).ok();
                    return Ok(());
                }
                b'q' => {
                    self.handle_query(&pkt[1..], &mut stream)?;
                }
                b'v' => {
                    self.handle_v_command(&pkt[1..], &mut stream)?;
                }
                _ => {
                    // Unknown command — empty response means "not supported"
                    rsp::send_empty(&mut stream)?;
                }
            }
        }
    }

    fn handle_read_single_register(
        &self,
        data: &[u8],
        stream: &mut TcpStream,
    ) -> std::io::Result<()> {
        let reg_num = match parse_hex_num(data) {
            Some(n) => n as usize,
            None => return rsp::send_error(stream, 1),
        };

        self.cmd_tx.send(DebugCommand::ReadRegisters).ok();
        match self.event_rx.recv() {
            Ok(DebugEvent::Registers(regs)) => {
                let val = match reg_num {
                    0 => regs.rax,
                    1 => regs.rbx,
                    2 => regs.rcx,
                    3 => regs.rdx,
                    4 => regs.rsi,
                    5 => regs.rdi,
                    6 => regs.rbp,
                    7 => regs.rsp,
                    8 => regs.r8,
                    9 => regs.r9,
                    10 => regs.r10,
                    11 => regs.r11,
                    12 => regs.r12,
                    13 => regs.r13,
                    14 => regs.r14,
                    15 => regs.r15,
                    16 => regs.rip,
                    17 => regs.eflags as u64,
                    18 => regs.cs as u64,
                    19 => regs.ss as u64,
                    20 => regs.ds as u64,
                    21 => regs.es as u64,
                    22 => regs.fs as u64,
                    23 => regs.gs as u64,
                    _ => {
                        // FP/SSE register — return zeros
                        // Registers 24-31 are st0-st7 (10 bytes each)
                        if reg_num >= 24 && reg_num <= 31 {
                            let zeros = vec![0u8; 10];
                            return rsp::send_packet(stream, &encode_hex(&zeros));
                        }
                        // 32-39 are fctrl..fop (4 bytes each)
                        if reg_num >= 32 && reg_num <= 39 {
                            let zeros = vec![0u8; 4];
                            return rsp::send_packet(stream, &encode_hex(&zeros));
                        }
                        // 40-55 are xmm0-xmm15 (16 bytes each)
                        if reg_num >= 40 && reg_num <= 55 {
                            let zeros = vec![0u8; 16];
                            return rsp::send_packet(stream, &encode_hex(&zeros));
                        }
                        // 56 is mxcsr (4 bytes)
                        if reg_num == 56 {
                            let zeros = vec![0u8; 4];
                            return rsp::send_packet(stream, &encode_hex(&zeros));
                        }
                        return rsp::send_error(stream, 14); // invalid register
                    }
                };
                // GP/segment regs: respond based on size
                if reg_num <= 16 {
                    rsp::send_packet(stream, &encode_hex(&val.to_le_bytes()))
                } else {
                    // eflags and segment regs are 4 bytes
                    rsp::send_packet(stream, &encode_hex(&(val as u32).to_le_bytes()))
                }
            }
            _ => rsp::send_error(stream, 1),
        }
    }

    fn handle_read_memory(&self, data: &[u8], stream: &mut TcpStream) -> std::io::Result<()> {
        // Format: addr,length (hex)
        let comma = data.iter().position(|&b| b == b',');
        let (addr_hex, len_hex) = match comma {
            Some(pos) => (&data[..pos], &data[pos + 1..]),
            None => return rsp::send_error(stream, 1),
        };
        let addr = match parse_hex_num(addr_hex) {
            Some(a) => a,
            None => return rsp::send_error(stream, 1),
        };
        let len = match parse_hex_num(len_hex) {
            Some(l) => l as usize,
            None => return rsp::send_error(stream, 1),
        };

        // Read via vCPU (which has access to CR3 for page table walks)
        self.cmd_tx
            .send(DebugCommand::ReadMemory { addr, len })
            .ok();
        match self.event_rx.recv() {
            Ok(DebugEvent::Memory(bytes)) => {
                rsp::send_packet(stream, &encode_hex(&bytes))
            }
            Ok(DebugEvent::Error(_)) => rsp::send_error(stream, 14),
            _ => rsp::send_error(stream, 1),
        }
    }

    fn handle_write_memory(&self, data: &[u8], stream: &mut TcpStream) -> std::io::Result<()> {
        // Format: addr,length:XX...
        let comma = data.iter().position(|&b| b == b',');
        let colon = data.iter().position(|&b| b == b':');
        let (addr_hex, _len_hex, hex_data) = match (comma, colon) {
            (Some(c), Some(co)) if c < co => (&data[..c], &data[c + 1..co], &data[co + 1..]),
            _ => return rsp::send_error(stream, 1),
        };
        let addr = match parse_hex_num(addr_hex) {
            Some(a) => a,
            None => return rsp::send_error(stream, 1),
        };
        let bytes = match decode_hex(hex_data) {
            Some(b) => b,
            None => return rsp::send_error(stream, 1),
        };

        self.cmd_tx
            .send(DebugCommand::WriteMemory { addr, data: bytes })
            .ok();
        match self.event_rx.recv() {
            Ok(DebugEvent::Ok) => rsp::send_ok(stream),
            _ => rsp::send_error(stream, 14),
        }
    }

    fn handle_insert_breakpoint(
        &mut self,
        data: &[u8],
        stream: &mut TcpStream,
    ) -> std::io::Result<()> {
        // Format: type,addr,kind
        let parts: Vec<&[u8]> = data.splitn(3, |&b| b == b',').collect();
        if parts.len() < 2 {
            return rsp::send_error(stream, 1);
        }
        let bp_type = match parse_hex_num(parts[0]) {
            Some(t) => t,
            None => return rsp::send_error(stream, 1),
        };
        let addr = match parse_hex_num(parts[1]) {
            Some(a) => a,
            None => return rsp::send_error(stream, 1),
        };

        match bp_type {
            0 => {
                // Software breakpoint
                self.cmd_tx
                    .send(DebugCommand::InsertSwBreakpoint(addr))
                    .ok();
                match self.event_rx.recv() {
                    Ok(DebugEvent::Ok) => rsp::send_ok(stream),
                    _ => rsp::send_error(stream, 14),
                }
            }
            _ => {
                // Hardware breakpoints not yet supported
                rsp::send_empty(stream)
            }
        }
    }

    fn handle_remove_breakpoint(
        &mut self,
        data: &[u8],
        stream: &mut TcpStream,
    ) -> std::io::Result<()> {
        let parts: Vec<&[u8]> = data.splitn(3, |&b| b == b',').collect();
        if parts.len() < 2 {
            return rsp::send_error(stream, 1);
        }
        let bp_type = match parse_hex_num(parts[0]) {
            Some(t) => t,
            None => return rsp::send_error(stream, 1),
        };
        let addr = match parse_hex_num(parts[1]) {
            Some(a) => a,
            None => return rsp::send_error(stream, 1),
        };

        match bp_type {
            0 => {
                self.cmd_tx
                    .send(DebugCommand::RemoveSwBreakpoint(addr))
                    .ok();
                match self.event_rx.recv() {
                    Ok(DebugEvent::Ok) => rsp::send_ok(stream),
                    _ => rsp::send_error(stream, 14),
                }
            }
            _ => rsp::send_empty(stream),
        }
    }

    fn handle_query(&self, data: &[u8], stream: &mut TcpStream) -> std::io::Result<()> {
        if data.starts_with(b"Supported") {
            rsp::send_packet(
                stream,
                b"PacketSize=4096;swbreak+;hwbreak+;vContSupported+",
            )
        } else if data.starts_with(b"Attached") {
            // We created the process (not attached to existing)
            rsp::send_packet(stream, b"1")
        } else if data.starts_with(b"fThreadInfo") {
            rsp::send_packet(stream, b"m1")
        } else if data.starts_with(b"sThreadInfo") {
            rsp::send_packet(stream, b"l")
        } else if data.starts_with(b"C") {
            rsp::send_packet(stream, b"QC1")
        } else if data.starts_with(b"Xfer:features:read:target.xml:") {
            let xml = r#"<?xml version="1.0"?><!DOCTYPE target SYSTEM "gdb-target.dtd"><target version="1.0"><architecture>i386:x86-64</architecture></target>"#;
            let response = format!("l{}", xml);
            rsp::send_packet(stream, response.as_bytes())
        } else if data.starts_with(b"TStatus") {
            // Tracepoints not supported
            rsp::send_empty(stream)
        } else if data.starts_with(b"Symbol") {
            rsp::send_ok(stream)
        } else {
            rsp::send_empty(stream)
        }
    }

    fn handle_v_command(&self, data: &[u8], stream: &mut TcpStream) -> std::io::Result<()> {
        if data.starts_with(b"Cont?") {
            // Report supported vCont actions
            rsp::send_packet(stream, b"vCont;c;s;t")
        } else if data.starts_with(b"Cont;c") {
            self.cmd_tx.send(DebugCommand::Continue).ok();
            self.wait_for_stop(stream)
        } else if data.starts_with(b"Cont;s") {
            self.cmd_tx.send(DebugCommand::SingleStep).ok();
            self.wait_for_stop(stream)
        } else if data.starts_with(b"Cont;t") {
            // Stop/pause — we're already stopped
            rsp::send_stop_trap(stream)
        } else if data.starts_with(b"MustReplyEmpty") {
            rsp::send_empty(stream)
        } else {
            rsp::send_empty(stream)
        }
    }

    /// Block until the vCPU reports a stop event, then send the appropriate stop reply.
    fn wait_for_stop(&self, stream: &mut TcpStream) -> std::io::Result<()> {
        // We need to handle both vCPU events and potential Ctrl+C from GDB.
        // For simplicity, just block on the event channel.
        match self.event_rx.recv() {
            Ok(DebugEvent::Stopped(reason)) => match reason {
                StopReason::Breakpoint => rsp::send_stop_trap(stream),
                StopReason::SingleStep => rsp::send_stop_trap(stream),
                StopReason::Signal(sig) => rsp::send_stop_signal(stream, sig),
                StopReason::Exited => {
                    // W00 = process exited with code 0
                    rsp::send_packet(stream, b"W00")
                }
            },
            Ok(_) => rsp::send_stop_trap(stream),
            Err(_) => {
                // Channel closed — vCPU thread died
                rsp::send_packet(stream, b"W00")
            }
        }
    }
}
