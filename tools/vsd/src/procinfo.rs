//! What `/proc` says about a veter window, for `vsd adopt` to tell
//! several apart (`doc/session-manager.md` §2.5).
//!
//! A pid alone names nothing a person recognises. What they recognise
//! is what they left running — the editor in one pane, the build in
//! another — and that is readable here without the window's help: every
//! terminal under it, at any depth, is a session whose leader's
//! `tpgid` is the job in its foreground.

use std::collections::HashMap;

/// The fields of `/proc/<pid>/stat` this module reads.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Stat {
    ppid: u32,
    session: u32,
    /// Foreground process group of the controlling terminal, or `-1`
    /// without one.
    tpgid: i32,
    /// Clock ticks after boot.
    starttime: u64,
}

/// Parse a `/proc/<pid>/stat` line. The command name sits in
/// parentheses and may itself contain spaces and parentheses, so the
/// fields are counted from the *last* `)`.
fn parse_stat(line: &str) -> Option<Stat> {
    let rest = &line[line.rfind(')')? + 2..];
    // `rest` starts at field 3 (state); fields are 1-based in proc(5).
    let f: Vec<&str> = rest.split_whitespace().collect();
    let field = |n: usize| f.get(n - 3).copied();
    Some(Stat {
        ppid: field(4)?.parse().ok()?,
        session: field(6)?.parse().ok()?,
        tpgid: field(8)?.parse().ok()?,
        starttime: field(22)?.parse().ok()?,
    })
}

fn stat(pid: u32) -> Option<Stat> {
    parse_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// `argv` of `pid`, its first word cut to a basename, at most `max`
/// characters.
fn command_line(pid: u32, max: usize) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let mut words = raw
        .split(|&b| b == 0)
        .filter(|w| !w.is_empty())
        .map(|w| String::from_utf8_lossy(w).into_owned());
    let first = words.next()?;
    let first = first.rsplit('/').next().unwrap_or(&first).to_string();
    let line = std::iter::once(first).chain(words).collect::<Vec<_>>().join(" ");
    Some(if line.chars().count() > max {
        let cut: String = line.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    } else {
        line
    })
}

/// Whether `ancestor` is `pid` or one of its ancestors.
pub fn is_descendant(pid: u32, ancestor: u32) -> bool {
    let mut p = pid;
    // Bounded: a pid recycled into its own ancestry would loop.
    for _ in 0..4096 {
        if p == ancestor {
            return true;
        }
        match stat(p) {
            Some(st) if st.ppid != 0 && st.ppid != p => p = st.ppid,
            _ => return false,
        }
    }
    false
}

/// How long `pid` has been running, in seconds.
pub fn age_secs(pid: u32) -> Option<u64> {
    let start = stat(pid)?.starttime;
    let uptime: f64 = std::fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    // SAFETY: plain sysconf query.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz <= 0 {
        return None;
    }
    Some((uptime - start as f64 / hz as f64).max(0.0) as u64)
}

/// The foreground job of every innermost terminal under `root`, as
/// short command lines — `["nvim src/main.rs", "cargo build", "zsh"]`
/// for a `vmux` with three panes.
///
/// "Innermost" because a multiplexer is the foreground of its own
/// terminal but says nothing a person would recognise; the panes it
/// runs are what they left behind. So a foreground job that has a
/// terminal session of its own somewhere below it is skipped for
/// what runs in that.
pub fn foreground_jobs(root: u32, max_each: usize) -> Vec<String> {
    let all: HashMap<u32, Stat> = std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let pid: u32 = e.file_name().to_str()?.parse().ok()?;
            Some((pid, stat(pid)?))
        })
        .collect();
    jobs_in(&all, root)
        .into_iter()
        .filter_map(|pid| command_line(pid, max_each))
        .collect()
}

/// [`foreground_jobs`] over an already-read process table, as pids.
fn jobs_in(all: &HashMap<u32, Stat>, root: u32) -> Vec<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, st) in all {
        children.entry(st.ppid).or_default().push(pid);
    }
    for list in children.values_mut() {
        list.sort_unstable();
    }
    let descendants = |from: u32| {
        let mut out = Vec::new();
        let mut stack = vec![from];
        while let Some(p) = stack.pop() {
            out.push(p);
            if let Some(c) = children.get(&p) {
                stack.extend(c.iter().rev());
            }
        }
        out
    };
    let is_leader = |p: &u32| all.get(p).is_some_and(|st| st.session == *p && st.tpgid > 0);

    let mut jobs = Vec::new();
    for leader in descendants(root).into_iter().filter(is_leader) {
        let fg = all[&leader].tpgid as u32;
        // A terminal session below the foreground job means it is a
        // multiplexer; its panes are listed on their own.
        let hosts_terminals = descendants(fg).iter().skip(1).any(is_leader);
        if !hosts_terminals && !jobs.contains(&fg) {
            jobs.push(fg);
        }
    }
    jobs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_survives_a_name_with_parens_and_spaces() {
        let line = "4242 (my (odd) name) S 1 4242 4242 34817 4300 4194560 \
                    0 0 0 0 0 0 0 0 20 0 1 0 987654 0 0";
        let st = parse_stat(line).unwrap();
        assert_eq!(st, Stat { ppid: 1, session: 4242, tpgid: 4300, starttime: 987654 });
    }

    fn st(ppid: u32, session: u32, tpgid: i32) -> Stat {
        Stat { ppid, session, tpgid, starttime: 0 }
    }

    #[test]
    fn a_multiplexer_reports_its_panes_not_itself() {
        // veter(1) → vmux(10, leads its pty, foreground of it) →
        //   zsh(20, idle) and zsh(30) running nvim(31).
        let all: HashMap<u32, Stat> = [
            (10, st(1, 10, 10)),
            (20, st(10, 20, 20)),
            (30, st(10, 30, 31)),
            (31, st(30, 30, 31)),
        ]
        .into();
        assert_eq!(jobs_in(&all, 10), vec![20, 31]);
    }

    #[test]
    fn we_descend_from_our_parent_but_not_from_a_child() {
        let me = std::process::id();
        assert!(is_descendant(me, std::os::unix::process::parent_id()));
        assert!(is_descendant(me, me));
        let child = std::process::Command::new("sleep").arg("5").spawn().unwrap();
        assert!(!is_descendant(me, child.id()));
        let _ = { let mut c = child; c.kill().and_then(|_| c.wait()) };
    }

    #[test]
    fn a_plain_shell_reports_its_foreground_job() {
        let all: HashMap<u32, Stat> = [(10, st(1, 10, 11)), (11, st(10, 10, 11))].into();
        assert_eq!(jobs_in(&all, 10), vec![11]);
    }
}
