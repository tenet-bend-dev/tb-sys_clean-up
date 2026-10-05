==============================================================
 TENET BEND - System Update & Clean-Up Utility  (sys_clean_up)
==============================================================

A command-line Windows maintenance tool written in Rust.
It cleans temporary files and caches, runs Disk Cleanup and DISM,
updates your programs through winget, and (optionally) resets the
network stack. No GUI - everything runs in the console.

Version : 2.0.0
Tested  : Windows 10 (build 19045). Windows 11 should work but is
          untested. Nothing in the code is tied to a Windows version.


--------------------------------------------------------------
 1. PROJECT LAYOUT
--------------------------------------------------------------

  Cargo.toml        Package manifest and dependencies
  build.rs          Embeds the app icon, version info and manifest
  assets\icon.ico   The application icon (build still works if missing)
  src\main.rs       The program

Dependencies: winreg, windows-sys. Build-only: winresource.
Embedding the icon needs rc.exe (MSVC toolchain) or windres (GNU).


--------------------------------------------------------------
 2. BUILD
--------------------------------------------------------------

  cargo build --release

The executable is created at:  target\release\sys_clean_up.exe

Quick safe test (changes nothing):

  cargo run -- --dry-run


--------------------------------------------------------------
 3. USAGE
--------------------------------------------------------------

  sys_clean_up.exe [OPTIONS]

The program asks for administrator rights (UAC) by itself and
relaunches with the same options.

  -n, --dry-run      Preview everything, change nothing
  -y, --yes          Skip the confirmation prompt
      --deep         Also clear memory dumps, minidumps, the Windows
                     Update datastore, and (via cleanmgr) Previous
                     Installations / Windows.old and upgrade leftovers
      --no-cleanmgr  Skip Windows Disk Cleanup
      --no-dism      Skip component store cleanup (DISM)
      --no-winget    Skip winget updates
      --store-reset  Also reset the Microsoft Store cache (wsreset -q)
      --net-reset    Run the network resets without asking
      --no-pause     Do not wait for Enter before closing
  -h, --help         Show the help text

Notes:
  * --yes does NOT run the network resets (they drop your
    connection). Only --net-reset, or answering "y", does.
  * Without --deep, nothing destructive beyond caches/temp files
    is touched.


--------------------------------------------------------------
 4. WHAT IT DOES (10 STEPS)
--------------------------------------------------------------

  1. Purges temp files and caches: Windows temp, user temp, local
     temp, crash dumps, D3D shader cache, Internet cache, error
     reports, thumbnail cache, CBS logs, Delivery Optimization
     cache and Prefetch. Locked files are retried, then skipped.
  2. Windows Update cache: stops wuauserv/bits (only if running),
     clears SoftwareDistribution\Download (and DataStore with
     --deep), then restarts only the services it stopped.
  3. Empties the Recycle Bin.
  4. Flushes the DNS cache (and resets the Store cache with
     --store-reset).
  5. Runs Windows Disk Cleanup (cleanmgr /sagerun:1) with only
     safe categories enabled. "User Profiles" is never enabled.
  6. DISM /StartComponentCleanup.
  7. Resets the BITS queue and triggers the Idle Maintenance task
     (skipped if the task does not exist).
  8. Writes a Local AppData manifest (names + last write times).
  9. winget: source update, version, installed list (saved as an
     inventory file), upgrade check, upgrade all, winget --info.
 10. Network maintenance: Winsock reset, TCP/IP reset, IP release
     and renew. Asks first. A restart is recommended afterwards.

At the end you get a summary (files, bytes, disk-space change,
warnings, duration) and an optional per-step log viewer.


--------------------------------------------------------------
 5. LOGS
--------------------------------------------------------------

Logs are written to a "logs" folder next to the executable:

  cleanup_YYYYMMDD_HHMMSS.log           Full timestamped log
  appdata_manifest_YYYYMMDD_HHMMSS.log  Local AppData snapshot
  package_inventory_YYYYMMDD_HHMMSS.log winget package list

Colors are stripped from the log files.


--------------------------------------------------------------
 6. SAFETY NOTES
--------------------------------------------------------------

  * Run with --dry-run first if you are unsure.
  * Symlinks and junctions are never followed during deletion.
  * Services are restored even if the program panics.
  * --deep removes Windows.old and similar rollback data
    (through cleanmgr). You cannot undo that.
  * winget upgrade --all runs WITHOUT --force on purpose, because
    --force can trigger reinstalls. Add "--force" to the winget
    upgrade arguments in main.rs if you want it.


--------------------------------------------------------------
 7. KNOWN BEHAVIOR (NOT BUGS)
--------------------------------------------------------------

  * cleanmgr opens its own small Windows progress window.
    Use --no-cleanmgr to skip it.
  * "netsh int ip reset" may report "Access is denied" for one
    protected registry key. This is a known Windows quirk; the
    rest is reset. Restart to apply.
  * Winsock/TCP-IP resets only fully apply after a restart.
  * A few files in use by running apps are always skipped.
  * "ipconfig /release" may say "media disconnected" for unplugged
    or unused adapters. This is harmless.
  * Some tools (cleanmgr, bitsadmin, parts of netsh) are being
    phased out by Microsoft. If one is missing, its step prints a
    warning and is skipped.


--------------------------------------------------------------
 8. EXIT CODES
--------------------------------------------------------------

  0  Finished (warnings are shown in the summary)
  1  Administrator elevation was declined or failed
  2  Unknown command-line option

==============================================================