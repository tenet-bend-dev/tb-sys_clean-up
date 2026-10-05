//! System Clean-Up - Windows maintenance and optimization utility.

use std::collections::HashSet;
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::thread;
use std::net::{SocketAddr, TcpStream};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows_sys::Win32::System::Console::{
    GetConsoleMode, GetStdHandle, SetConsoleMode, SetConsoleOutputCP, SetConsoleTitleW,
    ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::UI::Shell::{
    IsUserAnAdmin, SHEmptyRecycleBinW, ShellExecuteW, SHERB_NOCONFIRMATION, SHERB_NOPROGRESSUI,
    SHERB_NOSOUND,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use winreg::enums::*;
use winreg::RegKey;

// ───────────────────────────── Constants ─────────────────────────────

const TOTAL_STEPS: u32 = 10;
const INNER_WIDTH: usize = 61;
const RESET: &str = "\x1b[0m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const BLUE: &str = "\x1b[34m";
const PURPLE: &str = "\x1b[35m";
const CYAN: &str = "\x1b[36m";
const WHITE: &str = "\x1b[97m";
const GRAY: &str = "\x1b[90m";
const SAGE_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\VolumeCaches";

const SAGE_SAFE: &[&str] = &[
    "Active Setup Temp Folders",
    "BranchCache",
    "D3D Shader Cache",
    "Delivery Optimization Files",
    "Downloaded Program Files",
    "Internet Cache Files",
    "Old ChkDsk Files",
    "Recycle Bin",
    "Setup Log Files",
    "Temporary Files",
    "Temporary Setup Files",
    "Thumbnail Cache",
    "Update Cleanup",
    "Windows Defender",
    "Windows Error Reporting Files",
    "Windows Error Reporting Archive Files",
    "Windows Error Reporting Queue Files",
    "Windows Error Reporting System Archive Files",
    "Windows Error Reporting System Queue Files",
];

// Only enabled with --deep. "User Profiles" is intentionally never enabled.
const SAGE_DEEP: &[&str] = &[
    "Memory Dump Files",
    "Previous Installations",
    "Service Pack Cleanup",
    "System error memory dump files",
    "System error minidump files",
    "Upgrade Discarded Files",
    "Windows ESD installation files",
    "Windows Upgrade Log Files",
];

// ───────────────────────────── Config ─────────────────────────────

#[derive(Default, Clone)]
struct Config {
    dry_run: bool,
    yes: bool,
    deep: bool,
    no_winget: bool,
    no_cleanmgr: bool,
    no_dism: bool,
    no_pause: bool,
    store_reset: bool,
    net_reset: bool,
}

fn print_help() {
    println!(
        "System Clean-Up\n\n\
USAGE: sys_clean_up [OPTIONS]\n\n\
OPTIONS:\n\
  -n, --dry-run      Preview everything, change nothing\n\
  -y, --yes          Skip the confirmation prompt\n\
      --deep         Also clear memory dumps, Previous Installations, WU DataStore\n\
      --no-cleanmgr  Skip Windows Disk Cleanup\n\
      --no-dism      Skip component store cleanup (DISM)\n\
      --no-winget    Skip winget upgrades\n\
      --store-reset  Also reset the Microsoft Store cache (wsreset -q)\n\
      --net-reset    Run the Winsock/TCP-IP reset + IP release/renew without asking\n\
      --no-pause     Do not wait for Enter before closing\n\
  -h, --help         Show this help"
    );
}

fn parse_args() -> Result<Option<Config>, String> {
    let mut c = Config::default();
    for a in env::args().skip(1) {
        match a.to_lowercase().as_str() {
            "-n" | "--dry-run" => c.dry_run = true,
            "-y" | "--yes" => c.yes = true,
            "--deep" => c.deep = true,
            "--no-winget" => c.no_winget = true,
            "--no-cleanmgr" => c.no_cleanmgr = true,
            "--no-dism" => c.no_dism = true,
            "--no-pause" => c.no_pause = true,
            "--store-reset" => c.store_reset = true,
            "--net-reset" => c.net_reset = true,
            "-h" | "--help" => {
                print_help();
                return Ok(None);
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }
    Ok(Some(c))
}

// ───────────────────────────── Win32 helpers ─────────────────────────────

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

fn is_admin() -> bool {
    unsafe { IsUserAnAdmin() != 0 }
}

/// Relaunches this executable through UAC. Returns true if the prompt was accepted.
fn relaunch_elevated() -> bool {
    let Ok(exe) = env::current_exe() else { return false };
    let args: Vec<String> = env::args()
        .skip(1)
        .map(|a| if a.contains(' ') { format!("\"{a}\"") } else { a })
        .collect();
    let verb = wide(OsStr::new("runas"));
    let file = wide(exe.as_os_str());
    let params = wide(OsStr::new(&args.join(" ")));
    let dir = exe.parent().map(|p| wide(p.as_os_str()));
    let result = unsafe {
        ShellExecuteW(
            null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            dir.as_ref().map_or(null(), |d| d.as_ptr()),
            SW_SHOWNORMAL,
        )
    };
    (result as isize) > 32
}

fn enable_vt() -> bool {
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0u32;
        if GetConsoleMode(h, &mut mode) == 0 {
            return false;
        }
        SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}

fn free_space(root: &str) -> Option<u64> {
    let w = wide(OsStr::new(root));
    let (mut free, mut total, mut total_free) = (0u64, 0u64, 0u64);
    let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut free, &mut total, &mut total_free) };
    (ok != 0).then_some(free)
}

fn local_time() -> SYSTEMTIME {
    let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut st) };
    st
}

fn timestamp() -> String {
    let t = local_time();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

fn file_stamp() -> String {
    let t = local_time();
    format!(
        "{:04}{:02}{:02}_{:02}{:02}{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

fn accent_rgb() -> (u8, u8, u8) {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\DWM")
        .and_then(|k| k.get_value::<u32, _>("AccentColor"))
        .map(|v| ((v & 0xFF) as u8, ((v >> 8) & 0xFF) as u8, ((v >> 16) & 0xFF) as u8))
        .unwrap_or((0, 176, 240))
}

// ───────────────────────────── UI + logging ─────────────────────────────

struct Ui {
    color: bool,
    accent: String,
    log: Option<File>,
    warnings: u32,
    records: Vec<(String, Vec<String>)>,
    last_out: Vec<String>,
}

impl Ui {
    fn new(color: bool, log: Option<File>) -> Self {
        let (r, g, b) = accent_rgb();
        Self {
            color,
            accent: format!("\x1b[38;2;{r};{g};{b}m"),
            log,
            warnings: 0,
            records: Vec::new(),
            last_out: Vec::new(),
        }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.color { format!("{code}{text}{RESET}") } else { text.to_string() }
    }

    fn write_log(&mut self, level: &str, msg: &str) {
        let plain = strip_ansi(msg);
        if let Some(f) = self.log.as_mut() {
            let _ = writeln!(f, "[{}] [{level}] {plain}", timestamp());
        }
        if level != "STEP" {
            if let Some(r) = self.records.last_mut() {
                r.1.push(plain);
            }
        }
    }

    fn say(&self, s: &str) {
        if self.color {
            println!("{s}");
        } else {
            println!("{}", strip_ansi(s));
        }
    }

    fn raw(&self, s: &str) {
        if self.color {
            print!("{s}");
        } else {
            print!("{}", strip_ansi(s));
        }
        let _ = io::stdout().flush();
    }

    fn tagged(&mut self, level: &str, tag_color: &str, msg: &str) {
        self.say(&format!("{BLUE}[{tag_color}{level}{BLUE}]{RESET} {msg}"));
        self.write_log(level, msg);
    }

    fn info(&mut self, msg: &str) { self.tagged("INFO", CYAN, msg); }
    fn ok(&mut self, msg: &str) { self.tagged("OK", GREEN, msg); }

    fn warn(&mut self, msg: &str) {
        self.warnings += 1;
        self.tagged("WARN", YELLOW, msg);
    }

    fn fail(&mut self, msg: &str) {
        self.warnings += 1;
        self.tagged("FAIL", RED, msg);
    }

    fn dim(&mut self, msg: &str) {
        self.say(&format!("{GRAY}      {msg}{RESET}"));
        self.write_log("CMD", msg);
    }

    fn step(&mut self, n: u32, title: &str) {
        self.say("");
        self.say(&format!(
            "{BLUE}-{GREEN}-{BLUE}|{RED}| {YELLOW}STEP {GREEN}{n:02}{RESET}/{CYAN}{TOTAL_STEPS:02} {RESET}{title} {PURPLE}|{BLUE}|{CYAN}-{BLUE}-{RESET}"
        ));
        self.records.push((strip_ansi(title), Vec::new()));
        self.write_log("STEP", &format!("{n}/{TOTAL_STEPS} {title}"));
    }

    fn sub(&mut self, n: u32, label: &str) {
        self.say(&format!("{n}. {label}"));
        self.write_log("TASK", &format!("{n}. {label}"));
    }

    fn box_edge(&self, l: &str, r: &str) {
        println!("{}", self.paint(&self.accent, &format!("{l}{}{r}", "─".repeat(INNER_WIDTH))));
    }

    fn box_title(&self, title: &str) {
        let b = self.paint(&self.accent, "│");
        println!("{b}{}{b}", self.paint(WHITE, &format!("{title:^INNER_WIDTH$}")));
    }

    fn box_row(&self, label: &str, value: &str, value_color: &str) {
        let b = self.paint(&self.accent, "│");
        let l = format!("  {label:<14}");
        let used = l.chars().count() + value.chars().count();
        let pad = " ".repeat(INNER_WIDTH.saturating_sub(used));
        println!("{b}{}{}{pad}{b}", self.paint(GRAY, &l), self.paint(value_color, value));
    }
}

fn banner(ui: &Ui, cfg: &Config, log_name: &str) {
    ui.say(&format!(
        "{BLUE}-{GREEN}-{BLUE}|{RED}| {GREEN}TENET {YELLOW}BEND {RESET}⧉ {RED}System {CYAN}Update {YELLOW}& {PURPLE}Clean{RESET}-{PURPLE}Up {BLUE}Utility {PURPLE}|{BLUE}|{CYAN}-{BLUE}-{RESET}"
    ));
    ui.say("");
    let mode = if cfg.dry_run {
        format!("{YELLOW}DRY RUN {PURPLE}(NOTHING WILL BE CHANGED){RESET}")
    } else if cfg.deep {
        format!("{RED}DEEP {PURPLE}CLEAN{RESET}")
    } else {
        format!("{GREEN}STANDARD{RESET}")
    };
    ui.say(&format!("{GREEN}MODE{RESET}: {mode}"));
    ui.say(&format!("{GREEN}PRIVILEGE{RESET}: {BLUE}ADMINISTRATOR{RESET}"));
    ui.say(&format!("{GREEN}LOG FILE{RESET}: {BLUE}{log_name}{RESET}"));
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for d in chars.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Turns raw tool output into clean lines (drops spinner frames, keeps the last `\r` redraw).
fn clean_output(s: &str) -> Vec<String> {
    s.split('\n')
        .filter_map(|line| {
            let seg = line.rsplit('\r').find(|p| !p.trim().is_empty()).unwrap_or("").trim();
            let junk = seg.is_empty()
                || (seg.chars().count() <= 2 && seg.chars().all(|c| matches!(c, '-' | '\\' | '|' | '/' | ' ')));
            let bar = seg.starts_with('[') && seg.contains('%') && !seg.contains("100.0%");
            (!junk && !bar).then(|| seg.to_string())
        })
        .collect()
}

fn drain<R: Read + Send + 'static>(mut r: R) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut b = Vec::new();
        let _ = r.read_to_end(&mut b);
        b
    })
}

fn fmt_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < 4 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.2} {}", UNITS[i]) }
}

fn prompt(msg: &str) -> String {
    print!("{msg}");
    let _ = io::stdout().flush();
    let mut s = String::new();
    let _ = io::stdin().read_line(&mut s);
    s.trim().to_lowercase()
}

// ───────────────────────────── Command helpers ─────────────────────────────

/// Runs a command with a live `EXECUTING ....` animation. Output is captured, logged,
/// and echoed when `echo` is set (or when the command fails).
fn run_cmd(ui: &mut Ui, dry: bool, prog: &str, args: &[&str], echo: bool) -> bool {
    let shown = format!("{prog} {}", args.join(" "));
    if dry {
        ui.dim(&format!("(DRY-RUN) WOULD RUN: {shown}"));
        return true;
    }
    ui.dim(&format!("> {shown}"));
    let mut child = match Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            ui.fail(&format!("{RED}COULDN'T LAUNCH {CYAN}\"{prog}\"{RESET}: {e}"));
            return false;
        }
    };
    let out = child.stdout.take().map(drain);
    let err = child.stderr.take().map(drain);

    ui.raw(&format!("{YELLOW}EXECUTING {PURPLE}"));
    let mut ticks = 0u32;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                ui.raw(".");
                ticks += 1;
                if ticks % 30 == 0 {
                    ui.raw(&format!("\r{:<50}\r{YELLOW}EXECUTING {PURPLE}", ""));
                }
                thread::sleep(Duration::from_millis(500));
            }
            Err(_) => break None,
        }
    };
    ui.say(RESET);

    let stdout = out.map(|h| h.join().unwrap_or_default()).unwrap_or_default();
    let stderr = err.map(|h| h.join().unwrap_or_default()).unwrap_or_default();
    let mut lines = clean_output(&String::from_utf8_lossy(&stdout));
    lines.extend(clean_output(&String::from_utf8_lossy(&stderr)));

    ui.last_out = lines.clone();
    let success = status.map_or(false, |s| s.success());
    for l in &lines {
        if echo || !success {
            ui.say(&format!("{GRAY}{l}{RESET}"));
        }
        ui.write_log("OUT", l);
    }
    match status {
        Some(s) if s.success() => true,
        Some(s) => {
            let code = s.code().map_or_else(|| "?".to_string(), |c| c.to_string());
            ui.warn(&format!("{CYAN}{prog} {RED}EXITED WITH CODE {YELLOW}{code}{RESET}"));
            false
        }
        None => {
            ui.warn(&format!("{RED}COULD NOT READ THE STATUS OF {CYAN}{prog}{RESET}"));
            false
        }
    }
}

/// One numbered task: label, run, retry on failure.
#[allow(clippy::too_many_arguments)]
fn job(ui: &mut Ui, dry: bool, n: u32, label: &str, prog: &str, args: &[&str], echo: bool, retries: u8) -> bool {
    ui.sub(n, label);
    let mut left = retries;
    loop {
        if run_cmd(ui, dry, prog, args, echo) {
            if !dry {
                ui.ok(&format!("{GREEN}SUCCESS{RESET}"));
            }
            return true;
        }
        if left == 0 {
            ui.fail(&format!("{RED}FAILED {PURPLE}TO EXECUTE {CYAN}\"{prog}\"{RESET}. {CYAN}SKIPPING{RESET}..."));
            return false;
        }
        ui.say(&format!(
            "{YELLOW}RETRYING {CYAN}{n}. {PURPLE}({left} {} LEFT){RESET}...",
            if left == 1 { "RETRY" } else { "RETRIES" }
        ));
        left -= 1;
        thread::sleep(Duration::from_secs(2));
    }
}

fn quiet(prog: &str, args: &[&str]) -> bool {
    Command::new(prog)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn service_running(name: &str) -> bool {
    Command::new("sc")
        .args(["query", name])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("RUNNING"))
        .unwrap_or(false)
}

/// Stops services and guarantees only the ones it stopped are restarted on drop.
struct ServiceGuard {
    stopped: Vec<&'static str>,
}

impl ServiceGuard {
    fn stop(names: &[&'static str], dry: bool, ui: &mut Ui) -> Self {
        let mut stopped = Vec::new();
        for &n in names {
            if !service_running(n) {
                ui.info(&format!("{CYAN}{n}{RESET}: NOT RUNNING, {GREEN}LEAVING AS IS{RESET}"));
            } else if dry {
                ui.info(&format!("{YELLOW}(DRY-RUN){RESET} WOULD STOP {CYAN}{n}{RESET}"));
            } else if quiet("net", &["stop", n, "/y"]) {
                ui.ok(&format!("{CYAN}{n}{RESET}: {GREEN}STOPPED{RESET}"));
                stopped.push(n);
            } else {
                ui.warn(&format!("{CYAN}{n}{RESET}: {RED}COULD NOT BE STOPPED{RESET}, ITS FILES MAY BE SKIPPED"));
            }
        }
        Self { stopped }
    }
}

impl Drop for ServiceGuard {
    fn drop(&mut self) {
        for n in &self.stopped {
            quiet("net", &["start", n]);
        }
    }
}

// ───────────────────────────── File purging ─────────────────────────────

#[derive(Default, Clone, Copy)]
struct Stats {
    files: u64,
    bytes: u64,
    dirs: u64,
    locked: u64,
}

impl Stats {
    fn add(&mut self, o: &Stats) {
        self.files += o.files;
        self.bytes += o.bytes;
        self.dirs += o.dirs;
        self.locked += o.locked;
    }
}

enum Matcher {
    All,
    Ext(&'static str),
    Exact(&'static str),
    Thumbs,
}

struct Target {
    label: String,
    path: PathBuf,
    matcher: Matcher,
}

fn matches(m: &Matcher, name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    match m {
        Matcher::All => true,
        Matcher::Ext(e) => l.ends_with(e),
        Matcher::Exact(n) => l == *n,
        Matcher::Thumbs => {
            (l.starts_with("thumbcache_") || l.starts_with("iconcache_")) && l.ends_with(".db")
        }
    }
}

fn remove_file_forced(path: &Path, meta: &fs::Metadata) -> io::Result<()> {
    let mut perms = meta.permissions();
    if perms.readonly() {
        perms.set_readonly(false);
        let _ = fs::set_permissions(path, perms);
    }
    fs::remove_file(path)
}

/// Recursively empties `dir` (the directory itself is kept). Never follows symlinks/junctions.
fn purge_dir(dir: &Path, dry: bool, st: &mut Stats) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            if !dry && fs::remove_file(&path).is_err() && fs::remove_dir(&path).is_err() {
                st.locked += 1;
            }
            continue;
        }
        if ft.is_dir() {
            purge_dir(&path, dry, st);
            if dry || fs::remove_dir(&path).is_ok() {
                st.dirs += 1;
            }
        } else {
            let Ok(meta) = entry.metadata() else {
                st.locked += 1;
                continue;
            };
            let size = meta.len();
            if dry || remove_file_forced(&path, &meta).is_ok() {
                st.files += 1;
                st.bytes += size;
            } else {
                st.locked += 1;
            }
        }
    }
}

/// Top-level only: removes files in `dir` whose name satisfies the matcher.
fn purge_matching(dir: &Path, m: &Matcher, dry: bool, st: &mut Stats) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_file() || !matches(m, &entry.file_name().to_string_lossy()) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            st.locked += 1;
            continue;
        };
        let size = meta.len();
        if dry || remove_file_forced(&entry.path(), &meta).is_ok() {
            st.files += 1;
            st.bytes += size;
        } else {
            st.locked += 1;
        }
    }
}

fn purge(t: &Target, dry: bool, st: &mut Stats) {
    match t.matcher {
        Matcher::All => purge_dir(&t.path, dry, st),
        _ => purge_matching(&t.path, &t.matcher, dry, st),
    }
}

fn report(ui: &mut Ui, label: &str, st: &Stats, dry: bool) {
    let label = label.to_uppercase();
    if st.files == 0 && st.locked == 0 {
        ui.info(&format!("{CYAN}{label}{RESET}: {GREEN}ALREADY CLEAN{RESET}"));
        return;
    }
    let verb = if dry { "RECLAIMABLE" } else { "FREED" };
    let skipped = if st.locked > 0 {
        format!(" {YELLOW}({} IN USE/SKIPPED){RESET}", st.locked)
    } else {
        String::new()
    };
    ui.ok(&format!(
        "{CYAN}{label}{RESET}: {BLUE}{}{RESET} FILES, {GREEN}{}{RESET} {verb}{skipped}",
        st.files,
        fmt_bytes(st.bytes)
    ));
}

fn env_path(k: &str) -> Option<PathBuf> {
    env::var_os(k).map(PathBuf::from)
}

fn build_targets(deep: bool) -> Vec<Target> {
    let sys = env_path("SystemRoot").unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let local = env_path("LOCALAPPDATA");
    let pdata = env_path("ProgramData").unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    let mut v: Vec<Target> = Vec::new();
    let mut add = |label: &str, path: PathBuf, matcher: Matcher| {
        v.push(Target { label: label.to_string(), path, matcher });
    };

    add("Windows temp", sys.join("Temp"), Matcher::All);
    if let Some(t) = env_path("TEMP") {
        add("User temp", t, Matcher::All);
    }
    if let Some(l) = &local {
        add("Local temp", l.join("Temp"), Matcher::All);
        add("Crash dumps", l.join("CrashDumps"), Matcher::All);
        add("D3D shader cache", l.join("D3DSCache"), Matcher::All);
        add("Internet cache", l.join(r"Microsoft\Windows\INetCache"), Matcher::All);
        add("User error reports", l.join(r"Microsoft\Windows\WER"), Matcher::All);
        add("Thumbnail cache", l.join(r"Microsoft\Windows\Explorer"), Matcher::Thumbs);
    }
    add("System error report archive", pdata.join(r"Microsoft\Windows\WER\ReportArchive"), Matcher::All);
    add("System error report queue", pdata.join(r"Microsoft\Windows\WER\ReportQueue"), Matcher::All);
    add("System error report temp", pdata.join(r"Microsoft\Windows\WER\Temp"), Matcher::All);
    add("CBS logs", sys.join(r"Logs\CBS"), Matcher::Ext(".log"));
    add(
        "Delivery Optimization cache",
        sys.join(r"ServiceProfiles\NetworkService\AppData\Local\Microsoft\Windows\DeliveryOptimization\Cache"),
        Matcher::All,
    );
    add("Prefetch", sys.join("Prefetch"), Matcher::All);
    if deep {
        add("Minidumps", sys.join("Minidump"), Matcher::All);
        add("Full memory dump", sys.clone(), Matcher::Exact("memory.dmp"));
    }
    v
}

// ───────────────────────────── Disk Cleanup (cleanmgr) ─────────────────────────────

/// Enables only the selected caches (and clears stale flags from older runs).
fn configure_sageset(deep: bool) -> io::Result<u32> {
    let mut wanted: HashSet<String> = SAGE_SAFE.iter().map(|s| s.to_lowercase()).collect();
    if deep {
        wanted.extend(SAGE_DEEP.iter().map(|s| s.to_lowercase()));
    }
    let root = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(SAGE_KEY, KEY_READ)?;
    let mut enabled = 0;
    for name in root.enum_keys().flatten() {
        let Ok(k) = root.open_subkey_with_flags(&name, KEY_READ | KEY_SET_VALUE) else { continue };
        if wanted.contains(&name.to_lowercase()) {
            if k.set_value("StateFlags0001", &2u32).is_ok() {
                enabled += 1;
            }
        } else {
            let _ = k.delete_value("StateFlags0001");
        }
    }
    Ok(enabled)
}

// ───────────────────────────── Network / winget ─────────────────────────────

fn internet_ok() -> bool {
    "1.1.1.1:443"
        .parse::<SocketAddr>()
        .map(|a| TcpStream::connect_timeout(&a, Duration::from_secs(4)).is_ok())
        .unwrap_or(false)
}

fn have_winget() -> bool {
    quiet("winget", &["--version"])
}

fn open_log() -> (Option<File>, PathBuf, String) {
    let base = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("logs");
    let _ = fs::create_dir_all(&dir);
    let name = format!("cleanup_{}.log", file_stamp());
    let file = File::create(dir.join(&name)).ok();
    (file, dir, name)
}

// ───────────────────────────── Main flow ─────────────────────────────

fn main() -> ExitCode {
    let cfg = match parse_args() {
        Ok(Some(c)) => c,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e} (use --help)");
            return ExitCode::from(2);
        }
    };

    if !is_admin() {
        println!("[!] Requesting administrative elevation...");
        if relaunch_elevated() {
            return ExitCode::SUCCESS;
        }
        eprintln!("[x] Elevation was declined or failed. Administrator rights are required.");
        return ExitCode::from(1);
    }

    run_cleanup(&cfg)
}

fn run_cleanup(cfg: &Config) -> ExitCode {
    let started = Instant::now();
    unsafe {
        SetConsoleOutputCP(65001);
        SetConsoleTitleW(wide(OsStr::new("System Clean-Up")).as_ptr());
    }
    let color = enable_vt();
    let (log, log_dir, log_name) = open_log();
    let mut ui = Ui::new(color, log);
    if color {
        print!("\x1b[2J\x1b[H");
    }
    banner(&ui, cfg, &log_name);
    println!();

    let drive = env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
    let root = format!("{drive}\\");
    let free_before = free_space(&root);

    if !cfg.yes && !cfg.dry_run {
        if cfg.deep {
            ui.warn(&format!(
                "{RED}DEEP {PURPLE}MODE REMOVES MEMORY DUMPS, THE WU DATASTORE {RESET}AND {CYAN}(VIA CLEANMGR) PREVIOUS INSTALLATIONS / WINDOWS.OLD{RESET}."
            ));
        }
        let a = prompt(&format!(
            "{GREEN}PROCEED WITH THE CLEAN-UP{RESET}? ({BLUE}y{RESET}/{BLUE}n{RESET}) > "
        ));
        if a == "n" || a == "no" {
            ui.info(&format!("{YELLOW}ABORTED {CYAN}BY THE USER{RESET}."));
            finish(cfg);
            return ExitCode::SUCCESS;
        }
    }
    if let Some(f) = free_before {
        ui.info(&format!("{GREEN}FREE SPACE ON {CYAN}{drive} {GREEN}BEFORE{RESET}: {BLUE}{}{RESET}", fmt_bytes(f)));
    }

    let mut total = Stats::default();

    // 1 ── Temp files and caches
    ui.step(1, &format!("{GREEN}PURGING {CYAN}TEMPORARY FILES & CACHES{RESET}"));
    let mut seen = HashSet::new();
    for t in build_targets(cfg.deep) {
        let key = format!("{}|{}", t.path.to_string_lossy().to_lowercase(), matches!(t.matcher, Matcher::All));
        if !seen.insert(key) || !t.path.exists() {
            continue;
        }
        let mut st = Stats::default();
        purge(&t, cfg.dry_run, &mut st);
        let mut retries = 2u8;
        while st.locked > 0 && retries > 0 && !cfg.dry_run {
            ui.say(&format!(
                "{YELLOW}RETRYING {CYAN}{} {PURPLE}({retries} RETRIES LEFT){RESET}...",
                t.label.to_uppercase()
            ));
            thread::sleep(Duration::from_secs(1));
            let mut again = Stats::default();
            purge(&t, false, &mut again);
            st.files += again.files;
            st.bytes += again.bytes;
            st.dirs += again.dirs;
            st.locked = again.locked;
            retries -= 1;
        }
        report(&mut ui, &t.label, &st, cfg.dry_run);
        total.add(&st);
    }

    // 2 ── Windows Update cache
    ui.step(2, &format!("{GREEN}REFRESHING {CYAN}THE WINDOWS UPDATE CACHE{RESET}"));
    {
        let _guard = ServiceGuard::stop(&["wuauserv", "bits"], cfg.dry_run, &mut ui);
        let sys = env_path("SystemRoot").unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let sd = sys.join("SoftwareDistribution");
        let mut parts = vec![("Update downloads", sd.join("Download"))];
        if cfg.deep {
            parts.push(("Update datastore", sd.join("DataStore")));
        }
        for (label, path) in parts {
            if !path.exists() {
                continue;
            }
            let mut st = Stats::default();
            purge_dir(&path, cfg.dry_run, &mut st);
            report(&mut ui, label, &st, cfg.dry_run);
            total.add(&st);
        }
    }
    if !cfg.dry_run {
        ui.info(&format!("{CYAN}SERVICES {GREEN}RESTORED {RESET}TO THEIR PREVIOUS STATE"));
    }

    // 3 ── Recycle Bin
    ui.step(3, &format!("{GREEN}EMPTYING {CYAN}THE RECYCLE BIN{RESET}"));
    if cfg.dry_run {
        ui.info(&format!("{YELLOW}(DRY-RUN){RESET} WOULD EMPTY THE RECYCLE BIN ON ALL DRIVES"));
    } else {
        let hr = unsafe {
            SHEmptyRecycleBinW(null_mut(), null(), SHERB_NOCONFIRMATION | SHERB_NOPROGRESSUI | SHERB_NOSOUND)
        };
        if hr == 0 {
            ui.ok(&format!("{GREEN}RECYCLE BIN EMPTIED{RESET}"));
        } else if hr as u32 == 0x8000_FFFF {
            ui.info(&format!("{CYAN}RECYCLE BIN{RESET}: {GREEN}ALREADY EMPTY{RESET}"));
        } else {
            ui.info(&format!("{CYAN}RECYCLE BIN{RESET}: NOTHING TO EMPTY OR UNAVAILABLE {PURPLE}(0x{hr:08X}){RESET}"));
        }
    }

    // 4 ── DNS / Store
    ui.step(4, &format!("{GREEN}FLUSHING {CYAN}NETWORK CACHES{RESET}"));
    job(
        &mut ui, cfg.dry_run, 1,
        &format!("{GREEN}FLUSHING {CYAN}THE DNS RESOLVER CACHE{RESET}..."),
        "ipconfig", &["/flushdns"], true, 1,
    );
    if cfg.store_reset {
        job(
            &mut ui, cfg.dry_run, 2,
            &format!("{GREEN}RESETTING {CYAN}THE MICROSOFT STORE CACHE{RESET}..."),
            "wsreset.exe", &["-q"], false, 0,
        );
    }

    // 5 ── Disk Cleanup
    ui.step(5, &format!("{GREEN}RUNNING {CYAN}WINDOWS DISK CLEANUP {PURPLE}(CLEANMGR){RESET}"));
    if cfg.no_cleanmgr {
        ui.info(&format!("{CYAN}SKIPPED{RESET} (--no-cleanmgr)"));
    } else if cfg.dry_run {
        ui.info(&format!("{YELLOW}(DRY-RUN){RESET} WOULD CONFIGURE CLEANMGR AND RUN {CYAN}/sagerun:1{RESET}"));
    } else {
        match configure_sageset(cfg.deep) {
            Ok(n) => {
                ui.info(&format!("{BLUE}{n}{RESET} {CYAN}CLEANUP CATEGORIES ENABLED{RESET}"));
                job(
                    &mut ui, false, 1,
                    &format!("{GREEN}EXECUTING {CYAN}DISK CLEANUP{RESET}..."),
                    "cleanmgr", &["/sagerun:1"], false, 1,
                );
            }
            Err(e) => ui.fail(&format!("{RED}COULD NOT CONFIGURE CLEANMGR{RESET}: {e}")),
        }
    }

    // 6 ── Component store
    ui.step(6, &format!("{GREEN}CLEANING {CYAN}THE COMPONENT STORE {PURPLE}(DISM){RESET}"));
    if cfg.no_dism {
        ui.info(&format!("{CYAN}SKIPPED{RESET} (--no-dism)"));
    } else {
        job(
            &mut ui, cfg.dry_run, 1,
            &format!("{GREEN}RUNNING {CYAN}STARTCOMPONENTCLEANUP{RESET}..."),
            "dism.exe", &["/Online", "/Cleanup-Image", "/StartComponentCleanup"], false, 1,
        );
    }

    // 7 ── BITS reset + Idle Maintenance
    const IDLE_TASK: &str = r"\Microsoft\Windows\TaskScheduler\Idle Maintenance";
    ui.step(7, &format!("{GREEN}RESETTING {CYAN}BITS {RESET}& {CYAN}QUEUEING IDLE MAINTENANCE{RESET}"));
    job(
        &mut ui, cfg.dry_run, 1,
        &format!("{GREEN}RESETTING {CYAN}THE BITS QUEUE{RESET}..."),
        "bitsadmin", &["/reset", "/allusers"], false, 1,
    );
    if quiet("schtasks", &["/Query", "/TN", IDLE_TASK]) {
        job(
            &mut ui, cfg.dry_run, 2,
            &format!("{GREEN}TRIGGERING {CYAN}THE IDLE MAINTENANCE TASK{RESET}..."),
            "schtasks", &["/Run", "/TN", IDLE_TASK], true, 1,
        );
    } else {
        ui.info(&format!("{CYAN}IDLE MAINTENANCE TASK{RESET}: {YELLOW}NOT FOUND{RESET}, {CYAN}SKIPPING{RESET}"));
    }

    // 8 ── Local AppData manifest audit
    ui.step(8, &format!("{GREEN}AUDITING {CYAN}THE LOCAL APPDATA MANIFEST{RESET}"));
    if cfg.dry_run {
        ui.info(&format!("{YELLOW}(DRY-RUN){RESET} WOULD WRITE THE LOCAL APPDATA MANIFEST TO THE LOGS FOLDER"));
    } else {
        let path = log_dir.join(format!("appdata_manifest_{}.log", file_stamp()));
        match write_appdata_manifest(&path) {
            Ok(n) => ui.ok(&format!(
                "{BLUE}{n}{RESET} {CYAN}ENTRIES {GREEN}WRITTEN {RESET}TO {BLUE}{}{RESET}",
                path.display()
            )),
            Err(e) => ui.warn(&format!("{RED}COULD NOT WRITE THE MANIFEST{RESET}: {e}")),
        }
    }

    // 9 ── Winget
    ui.step(9, &format!("{GREEN}UPDATING {CYAN}THIRD-PARTY PACKAGES {PURPLE}(WINGET){RESET}"));
    if cfg.no_winget {
        ui.info(&format!("{CYAN}SKIPPED{RESET} (--no-winget)"));
    } else if !have_winget() {
        ui.warn(&format!("{RED}WINGET {PURPLE}WAS NOT FOUND{RESET}. {CYAN}SKIPPING{RESET}..."));
    } else if !internet_ok() {
        ui.warn(&format!("{RED}NO INTERNET CONNECTION {PURPLE}DETECTED{RESET}. {CYAN}SKIPPING WINGET{RESET}..."));
    } else {
        let d = cfg.dry_run;
        job(&mut ui, d, 0, &format!("{GREEN}UPDATING {CYAN}THE WINGET SOURCE{RESET}..."),
            "winget", &["source", "update"], false, 1);
        job(&mut ui, d, 1, &format!("{GREEN}CHECKING {CYAN}WINGET VERSION{RESET}..."),
            "winget", &["--version"], true, 1);
        let listed = job(&mut ui, d, 2, &format!("{GREEN}LISTING {CYAN}INSTALLED PROGRAMS{RESET}..."),
            "winget", &["list", "--accept-source-agreements"], true, 1);
        if !d && listed {
            let inv = log_dir.join(format!("package_inventory_{}.log", file_stamp()));
            match write_lines(&inv, &ui.last_out) {
                Ok(()) => ui.ok(&format!("{GREEN}PACKAGE INVENTORY SAVED{RESET}: {BLUE}{}{RESET}", inv.display())),
                Err(e) => ui.warn(&format!("{RED}COULD NOT SAVE THE PACKAGE INVENTORY{RESET}: {e}")),
            }
        }
        job(&mut ui, d, 3, &format!("{GREEN}CHECKING {CYAN}FOR UPGRADES{RESET}..."),
            "winget", &["upgrade", "--include-unknown"], true, 1);
        job(&mut ui, d, 4, &format!("{GREEN}UPGRADING {CYAN}PACKAGES{RESET}..."),
            "winget",
            &["upgrade", "--all", "--include-unknown", "--silent",
              "--accept-source-agreements", "--accept-package-agreements"],
            true, 1);
        job(&mut ui, d, 5, &format!("{GREEN}DISPLAYING {CYAN}WINGET INFO{RESET}..."),
            "winget", &["--info"], true, 1);
    }

    // 10 ── Network maintenance
    ui.step(10, &format!("{GREEN}NETWORK {CYAN}MAINTENANCE{RESET}"));
    let go_net = if cfg.dry_run || cfg.net_reset {
        true
    } else if cfg.yes {
        false
    } else {
        ui.say(&format!(
            "{YELLOW}NOTE{RESET}: {CYAN}WINSOCK / TCP-IP RESETS MAY REQUIRE A SYSTEM RESTART{RESET}, {CYAN}AND THE CONNECTION DROPS BRIEFLY{RESET}."
        ));
        let a = prompt(&format!("{GREEN}PROCEED{RESET}? ({BLUE}y{RESET}/{BLUE}n{RESET}) > "));
        a == "y" || a == "yes"
    };
    if go_net {
        let d = cfg.dry_run;
        job(&mut ui, d, 1, &format!("{GREEN}EXECUTING {CYAN}WINSOCK RESET{RESET}..."),
            "netsh", &["winsock", "reset"], true, 1);
        let ip_ok = job(&mut ui, d, 2, &format!("{GREEN}RESETTING {CYAN}THE TCP/IP CONFIGURATION{RESET}..."),
            "netsh", &["int", "ip", "reset"], true, 0);
        if !ip_ok && !d && ui.last_out.iter().any(|l| l.contains("Restart the computer")) {
            ui.info(&format!(
                "{YELLOW}ONE PROTECTED REGISTRY KEY {CYAN}REFUSED THE RESET {PURPLE}(KNOWN WINDOWS BEHAVIOR){RESET}. {CYAN}EVERYTHING ELSE WAS RESET{RESET}; {GREEN}RESTART{RESET} TO APPLY."
            ));
        }
        job(&mut ui, d, 3, &format!("{GREEN}EXECUTING {CYAN}\"ipconfig /release\"{RESET}..."),
            "ipconfig", &["/release"], true, 1);
        job(&mut ui, d, 4, &format!("{GREEN}RENEWING {CYAN}THE DHCP LEASE{RESET}..."),
            "ipconfig", &["/renew"], true, 1);
        if !d {
            ui.info(&format!("{YELLOW}A RESTART {CYAN}IS RECOMMENDED {RESET}TO FULLY APPLY THE RESETS"));
        }
    } else {
        ui.info(&format!("{CYAN}SKIPPED{RESET} (USE {YELLOW}--net-reset{RESET} TO RUN IT UNATTENDED)"));
    }

    // Summary
    println!();
    let free_after = free_space(&root);
    ui.box_edge("┌", "┐");
    ui.box_title("SUMMARY");
    ui.box_edge("├", "┤");
    let verb = if cfg.dry_run { "Reclaimable:" } else { "Deleted:" };
    ui.box_row(verb, &format!("{} in {} files", fmt_bytes(total.bytes), total.files), GREEN);
    ui.box_row("Skipped:", &format!("{} locked / in-use items", total.locked), YELLOW);
    if let (Some(b), Some(a)) = (free_before, free_after) {
        let delta = if a >= b {
            format!("+{}", fmt_bytes(a - b))
        } else {
            format!("-{}", fmt_bytes(b - a))
        };
        ui.box_row("Disk change:", &format!("{delta} (now {} free)", fmt_bytes(a)), WHITE);
    }
    ui.box_row("Warnings:", &ui.warnings.to_string(), if ui.warnings > 0 { YELLOW } else { GREEN });
    ui.box_row("Duration:", &format!("{:.1}s", started.elapsed().as_secs_f32()), WHITE);
    ui.box_edge("└", "┘");
    ui.write_log(
        "DONE",
        &format!("{} files, {} bytes, {} locked, {} warnings", total.files, total.bytes, total.locked, ui.warnings),
    );

    ui.say(&format!("{GREEN}LOG SAVED TO{RESET}: {BLUE}{}{RESET}", log_dir.join(&log_name).display()));
    ui.say(&format!("{GREEN}SYSTEM {BLUE}CLEAN{RESET}-{BLUE}UP {GREEN}IS COMPLETE{RED}!{RESET}"));

    if !cfg.no_pause {
        log_menu(&ui);
    }
    finish(cfg);
    ExitCode::SUCCESS
}

fn write_lines(path: &Path, lines: &[String]) -> io::Result<()> {
    let mut f = File::create(path)?;
    for l in lines {
        writeln!(f, "{l}")?;
    }
    Ok(())
}

/// Civil date from a SystemTime, in UTC (no extra crates needed).
fn fmt_utc(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// Name + last-write-time snapshot of %LOCALAPPDATA% (replaces the PowerShell Get-ChildItem call).
fn write_appdata_manifest(path: &Path) -> io::Result<usize> {
    let base = env_path("LOCALAPPDATA")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA is not set"))?;
    let mut rows: Vec<(String, String)> = fs::read_dir(&base)?
        .flatten()
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let when = e
                .metadata()
                .and_then(|m| m.modified())
                .map(fmt_utc)
                .unwrap_or_else(|_| "unknown".to_string());
            (name, when)
        })
        .collect();
    rows.sort_by_key(|r| r.0.to_lowercase());
    let mut f = File::create(path)?;
    writeln!(f, "{:<48} LastWriteTime (UTC)", "Name")?;
    writeln!(f, "{:<48} ------------------", "----")?;
    for (n, t) in &rows {
        writeln!(f, "{n:<48} {t}")?;
    }
    Ok(rows.len())
}

fn log_menu(ui: &Ui) {
    if ui.records.is_empty() {
        return;
    }
    println!();
    let ans = prompt(&format!(
        "{GREEN}WOULD YOU LIKE TO VIEW THE LOG OF A SPECIFIC STEP {RESET}({BLUE}y{RESET}/{BLUE}n{RESET})? > "
    ));
    if ans != "y" && ans != "yes" {
        return;
    }
    loop {
        ui.say("");
        ui.say(&format!("{BLUE}-{GREEN}-{BLUE}|{RED}| {RED}L{GREEN}O{PURPLE}G {PURPLE}|{BLUE}|{CYAN}-{BLUE}-{RESET}"));
        for (i, (title, _)) in ui.records.iter().enumerate() {
            ui.say(&format!("{YELLOW}{}{RESET}. {CYAN}{title}{RESET}", i + 1));
        }
        ui.say(&format!("{YELLOW}0{RESET}. {CYAN}CANCEL SELECTION{RESET}"));
        let pick = prompt(&format!("{GREEN}ENTER A NUMBER{RESET} > "));
        match pick.parse::<usize>() {
            Ok(0) => return,
            Ok(n) if n <= ui.records.len() => {
                let (title, lines) = &ui.records[n - 1];
                ui.say(&format!("{GREEN}LOG{RESET}: {BLUE}{title}{RESET}"));
                for l in lines {
                    ui.say(&format!("{GRAY}{l}{RESET}"));
                }
            }
            _ => ui.say(&format!("{RED}INVALID SELECTION{RESET}")),
        }
    }
}

fn finish(cfg: &Config) {
    if !cfg.no_pause {
        println!();
        prompt(&format!("{CYAN}PRESS {GREEN}ENTER {CYAN}TO CLOSE{RESET}... "));
    }
}