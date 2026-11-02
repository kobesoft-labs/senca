//! codec worker のサンドボックス（Linux、仕様 9.4「codec worker は権限・filesystem・network・CPU/memory を制限する」）。
//!
//! # なぜ `unsafe` か
//!
//! prctl・setrlimit・seccomp・Landlock・dup2 はどれも libc の生のシステムコールで、安全なラッパーが標準に無い。
//! 依存を増やさない（ADR-0002）ため、このモジュールだけ `#[allow(unsafe_code)]` にして `libc` で直接呼ぶ。
//! 各 `unsafe` には前提（SAFETY）を書く。ここ以外のモジュールは `unsafe` 禁止のまま。
//!
//! # 適用するもの（順に）
//!
//! 1. 早い段階（コーデックのライブラリを読み込む前、スレッドを作る前）
//!    - 標準出力の隔離: プロトコル用に fd 1 を複製し、fd 1 は標準エラーに付け替える（ライブラリが
//!      printf してもプロトコルを壊さない）。
//!    - `PR_SET_PDEATHSIG = SIGKILL`（親が死んだら一緒に終わる）、`PR_SET_DUMPABLE = 0`（同じ利用者からの ptrace を防ぐ）。
//!    - `RLIMIT_CORE = 0`、`RLIMIT_NOFILE`。
//!    - `PR_SET_NO_NEW_PRIVS`（setuid などで権限を得られない。seccomp・Landlock の前提）。
//!    - Landlock（使えるカーネルなら）: ファイルの作成・書き込み・削除を `/dev` の下（GPU の装置ファイル）以外で禁止する。
//!      読み取りは制限しない（コーデック・ドライバーのライブラリを dlopen するため）。Landlock は呼んだスレッドにしか
//!      効かないので、スレッドを作る前に適用する。
//! 2. 検出（ライブラリの読み込み）の後
//!    - `RLIMIT_AS`（指定があるとき。NVIDIA が使えるときは CUDA が大きな仮想アドレスを予約するので掛けない）。
//!    - seccomp-BPF の拒否リスト（`SECCOMP_FILTER_FLAG_TSYNC` で全スレッドに）。ネットワーク（socket・connect・bind・
//!      listen・accept）、プログラムの実行（execve・execveat）、ptrace・他プロセスのメモリ、mount 系、カーネルモジュール、
//!      kexec、bpf、鍵束、名前空間（unshare・setns・名前空間を作る clone）、reboot・swap・時計の設定などを `EPERM` にする。
//!      `clone3` は引数を検査できないので `ENOSYS` を返し、glibc に `clone` へ戻させる。アーキテクチャが違えば強制終了。
//!      ファイルを開く・mmap は許す（ジョブの途中で dlopen・GPU の装置を開くことがあるため）。
//! 3. ジョブごと
//!    - `RLIMIT_CPU` のソフト上限を「これまでの使用量 + ジョブあたりの上限」に設定する（超えると SIGXCPU で終わり、
//!      親が作り直す）。壁時計の時間切れは親（`WorkerPool`）が見る。
//!
//! # 掛けないもの
//!
//! - `RLIMIT_NPROC` は利用者単位の数でスレッドも数えるため、同じ利用者の他のプロセスに左右される。掛けない。
//! - `RLIMIT_FSIZE` は親から受け継いだ標準エラーが通常ファイルのとき書き込みで SIGXFSZ になるため掛けない
//!   （新しいファイルの書き込みは Landlock で止める）。
//! - socket の拒否により、NVIDIA の persistenced への接続など一部のドライバーの補助機能は使えない
//!   （ドライバーは無くても動く）。実機での確認は未了。

use std::fs::File;
use std::os::fd::FromRawFd;
use std::path::Path;

/// サンドボックスの設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxConfig {
    /// `false` ならサンドボックスを掛けない（調査用）。標準出力の隔離だけは行う。
    pub enabled: bool,
    /// `RLIMIT_AS`（MiB）。
    pub max_memory_mb: Option<u64>,
    /// ジョブあたりの CPU 時間（秒）。
    pub max_cpu_secs_per_job: Option<u64>,
    /// `RLIMIT_NOFILE`。
    pub max_open_files: u64,
    pub landlock: bool,
    pub seccomp: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        SandboxConfig {
            enabled: true,
            max_memory_mb: None,
            max_cpu_secs_per_job: Some(300),
            max_open_files: 256,
            landlock: true,
            seccomp: true,
        }
    }
}

/// 適用した結果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxState {
    pub no_new_privs: bool,
    pub landlock: bool,
    pub seccomp: bool,
    pub notes: Vec<String>,
}

impl SandboxState {
    pub fn summary(&self) -> String {
        self.notes.join("; ")
    }
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// プロトコル用に標準出力を複製し、fd 1 を標準エラーに付け替える。
pub fn isolate_stdout() -> std::io::Result<File> {
    // SAFETY: fcntl(F_DUPFD_CLOEXEC) は fd 1 を複製するだけで、メモリに触れない。
    let fd = unsafe { libc::fcntl(1, libc::F_DUPFD_CLOEXEC, 3) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: dup2 は fd 1 を fd 2 の複製に置き換える。fd 1 を所有する Rust のオブジェクトは無い
    // （std の Stdout は fd 1 を借りるだけで、以後の書き込みは標準エラーへ行く）。
    if unsafe { libc::dup2(2, 1) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` は上で作った、ほかに所有者のいない開いた fd。
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn set_rlimit(
    resource: libc::__rlimit_resource_t,
    soft: u64,
    hard: Option<u64>,
) -> Result<(), i32> {
    let mut cur = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `cur` は有効な書き込み先。
    if unsafe { libc::getrlimit(resource, &mut cur) } != 0 {
        return Err(errno());
    }
    let max = hard.unwrap_or(cur.rlim_max).min(cur.rlim_max);
    let new = libc::rlimit {
        rlim_cur: soft.min(max),
        rlim_max: max,
    };
    // SAFETY: `new` は有効な読み取り元。上限を下げるだけ（上げるには権限が要り、失敗するだけ）。
    if unsafe { libc::setrlimit(resource, &new) } != 0 {
        return Err(errno());
    }
    Ok(())
}

/// これまでに使った CPU 時間（秒、切り上げ）。
pub fn cpu_seconds_used() -> u64 {
    // SAFETY: rusage は全ビット 0 で有効な値。
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `ru` は有効な書き込み先。
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) } != 0 {
        return 0;
    }
    let s = ru.ru_utime.tv_sec as u64 + ru.ru_stime.tv_sec as u64;
    s + 1
}

/// ジョブの前に CPU 時間の上限を掛け直す。
pub fn arm_cpu_limit(per_job_secs: u64) {
    let _ = set_rlimit(
        libc::RLIMIT_CPU,
        cpu_seconds_used().saturating_add(per_job_secs),
        None,
    );
}

/// 早い段階の制限（ライブラリを読み込む前、スレッドを作る前に呼ぶ）。
pub fn apply_early(cfg: &SandboxConfig) -> SandboxState {
    let mut st = SandboxState::default();
    if !cfg.enabled {
        st.notes.push("sandbox disabled".into());
        return st;
    }
    // SAFETY: prctl の整数引数だけの操作。
    unsafe {
        libc::prctl(
            libc::PR_SET_PDEATHSIG,
            libc::SIGKILL as libc::c_ulong,
            0,
            0,
            0,
        );
        libc::prctl(libc::PR_SET_DUMPABLE, 0 as libc::c_ulong, 0, 0, 0);
    }
    match set_rlimit(libc::RLIMIT_CORE, 0, Some(0)) {
        Ok(()) => st.notes.push("core=0".into()),
        Err(e) => st.notes.push(format!("core limit failed (errno {e})")),
    }
    match set_rlimit(
        libc::RLIMIT_NOFILE,
        cfg.max_open_files,
        Some(cfg.max_open_files),
    ) {
        Ok(()) => st.notes.push(format!("nofile={}", cfg.max_open_files)),
        Err(e) => st.notes.push(format!("nofile limit failed (errno {e})")),
    }
    // SAFETY: prctl の整数引数だけの操作。
    st.no_new_privs =
        unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1 as libc::c_ulong, 0, 0, 0) } == 0;
    st.notes.push(if st.no_new_privs {
        "no_new_privs".into()
    } else {
        format!("no_new_privs failed (errno {})", errno())
    });
    if cfg.landlock && st.no_new_privs {
        match landlock_deny_writes(&[Path::new("/dev")]) {
            Ok(abi) => {
                st.landlock = true;
                st.notes
                    .push(format!("landlock(abi {abi}): writes only under /dev"));
            }
            Err(e) => st.notes.push(format!("landlock skipped: {e}")),
        }
    }
    st
}

/// 検出の後の制限。`gpu_runtime` は CUDA など大きな仮想アドレスを予約する実行環境が使えるとき。
pub fn apply_late(cfg: &SandboxConfig, st: &mut SandboxState, gpu_runtime: bool) {
    if !cfg.enabled {
        return;
    }
    if let Some(mb) = cfg.max_memory_mb {
        if gpu_runtime {
            st.notes
                .push("address-space limit skipped (GPU runtime present)".into());
        } else {
            let bytes = mb.saturating_mul(1024 * 1024);
            match set_rlimit(libc::RLIMIT_AS, bytes, Some(bytes)) {
                Ok(()) => st.notes.push(format!("as={mb}MiB")),
                Err(e) => st.notes.push(format!("as limit failed (errno {e})")),
            }
        }
    }
    if let Some(s) = cfg.max_cpu_secs_per_job {
        arm_cpu_limit(s);
        st.notes.push(format!("cpu={s}s/job"));
    }
    if cfg.seccomp {
        if !st.no_new_privs {
            st.notes
                .push("seccomp skipped (no_new_privs not set)".into());
        } else {
            match install_seccomp() {
                Ok(()) => {
                    st.seccomp = true;
                    st.notes.push("seccomp deny-list".into());
                }
                Err(e) => st.notes.push(format!("seccomp failed: {e}")),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Landlock

const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;
const ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
const ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
const ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
const ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
const ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
const ACCESS_FS_MAKE_REG: u64 = 1 << 8;
const ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
const ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
const ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
const ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
const ACCESS_FS_REFER: u64 = 1 << 13;
const ACCESS_FS_TRUNCATE: u64 = 1 << 14;

#[repr(C)]
struct LandlockRulesetAttr {
    handled_access_fs: u64,
}

#[repr(C, packed)]
struct LandlockPathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// 書き込み系の操作を `allow_write` の下の既存ファイルへの書き込み以外で禁止する。成功すれば ABI の版を返す。
fn landlock_deny_writes(allow_write: &[&Path]) -> Result<i64, String> {
    // SAFETY: 版の問い合わせ（attr = NULL, size = 0）。メモリを渡さない。
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<LandlockRulesetAttr>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if abi < 1 {
        return Err(format!("not supported by kernel (errno {})", errno()));
    }
    let mut handled = ACCESS_FS_WRITE_FILE
        | ACCESS_FS_REMOVE_DIR
        | ACCESS_FS_REMOVE_FILE
        | ACCESS_FS_MAKE_CHAR
        | ACCESS_FS_MAKE_DIR
        | ACCESS_FS_MAKE_REG
        | ACCESS_FS_MAKE_SOCK
        | ACCESS_FS_MAKE_FIFO
        | ACCESS_FS_MAKE_BLOCK
        | ACCESS_FS_MAKE_SYM;
    let mut allowed = ACCESS_FS_WRITE_FILE;
    if abi >= 2 {
        handled |= ACCESS_FS_REFER;
    }
    if abi >= 3 {
        handled |= ACCESS_FS_TRUNCATE;
        allowed |= ACCESS_FS_TRUNCATE;
    }
    let attr = LandlockRulesetAttr {
        handled_access_fs: handled,
    };
    // SAFETY: `attr` は有効で、大きさを正しく渡す（ABI 1 の構造体。新しいカーネルも小さい構造体を受け付ける）。
    let ruleset = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr as *const LandlockRulesetAttr,
            std::mem::size_of::<LandlockRulesetAttr>(),
            0u32,
        )
    };
    if ruleset < 0 {
        return Err(format!("create_ruleset failed (errno {})", errno()));
    }
    let ruleset = ruleset as libc::c_int;
    let close = |fd: libc::c_int| {
        // SAFETY: 自分で開いた fd を閉じる。
        unsafe { libc::close(fd) };
    };
    for dir in allow_write {
        let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()) else {
            continue;
        };
        // SAFETY: NUL 終端のパス。O_PATH で開くだけ。
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if fd < 0 {
            continue;
        }
        let rule = LandlockPathBeneathAttr {
            allowed_access: allowed,
            parent_fd: fd,
        };
        // SAFETY: `rule` は有効な packed 構造体（カーネルの定義と同じ配置）。
        let r = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                ruleset,
                LANDLOCK_RULE_PATH_BENEATH,
                &rule as *const LandlockPathBeneathAttr,
                0u32,
            )
        };
        close(fd);
        if r != 0 {
            let e = errno();
            close(ruleset);
            return Err(format!("add_rule failed (errno {e})"));
        }
    }
    // SAFETY: 自分で作った ruleset の fd を渡す。
    let r = unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0u32) };
    let e = errno();
    close(ruleset);
    if r != 0 {
        return Err(format!("restrict_self failed (errno {e})"));
    }
    Ok(abi)
}

// ---------------------------------------------------------------------------
// seccomp

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;

const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JEQ_K: u16 = 0x15;
const BPF_JSET_K: u16 = 0x45;
const BPF_RET_K: u16 = 0x06;
const RET_ALLOW: u32 = 0x7fff_0000;
const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_ERRNO: u32 = 0x0005_0000;
/// 名前空間を作る clone のフラグ（NEWNS・NEWCGROUP・NEWUTS・NEWIPC・NEWUSER・NEWPID・NEWNET・NEWTIME）。
const CLONE_NEW_MASK: u32 = 0x7E02_0080;

/// 拒否するシステムコール。
fn denied_syscalls() -> Vec<libc::c_long> {
    #[allow(unused_mut)]
    let mut v: Vec<libc::c_long> = vec![
        libc::SYS_socket,
        libc::SYS_socketpair,
        libc::SYS_connect,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_accept,
        libc::SYS_accept4,
        libc::SYS_execve,
        libc::SYS_execveat,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_kexec_load,
        libc::SYS_kexec_file_load,
        libc::SYS_bpf,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_reboot,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_perf_event_open,
        libc::SYS_userfaultfd,
        libc::SYS_open_by_handle_at,
        libc::SYS_name_to_handle_at,
        libc::SYS_fanotify_init,
        libc::SYS_acct,
        libc::SYS_settimeofday,
        libc::SYS_clock_settime,
        libc::SYS_sethostname,
        libc::SYS_setdomainname,
        libc::SYS_quotactl,
    ];
    #[cfg(target_arch = "x86_64")]
    v.extend([libc::SYS_iopl, libc::SYS_ioperm]);
    v
}

fn stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

fn jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

/// BPF のプログラムを組み立てる。
fn build_filter() -> Vec<libc::sock_filter> {
    let denied = denied_syscalls();
    let mut p = vec![
        stmt(BPF_LD_W_ABS, 4), // arch
        jump(BPF_JEQ_K, AUDIT_ARCH, 1, 0),
        stmt(BPF_RET_K, RET_KILL_PROCESS),
        stmt(BPF_LD_W_ABS, 0), // nr
    ];
    // 以降の命令の位置（BPF は前方にしか飛べない）:
    // [x32 検査?][拒否 n 個][clone3][clone][CHECK: ld args[0]][CHECK: jset][ALLOW][DENY][ENOSYS]
    let x32 = cfg!(target_arch = "x86_64") as usize;
    let clone3_at = p.len() + x32 + denied.len();
    let clone_at = clone3_at + 1;
    let check_ld = clone_at + 1;
    let check_jset = clone_at + 2;
    let allow_at = clone_at + 3;
    let deny_at = allow_at + 1;
    let enosys_at = allow_at + 2;
    let rel = |from: usize, to: usize| -> u8 {
        u8::try_from(to - from - 1).expect("filter jump fits in u8")
    };
    if x32 == 1 {
        let i = p.len();
        p.push(jump(BPF_JSET_K, 0x4000_0000, rel(i, deny_at), 0));
    }
    for nr in denied {
        let i = p.len();
        p.push(jump(BPF_JEQ_K, nr as u32, rel(i, deny_at), 0));
    }
    debug_assert_eq!(p.len(), clone3_at);
    p.push(jump(
        BPF_JEQ_K,
        libc::SYS_clone3 as u32,
        rel(clone3_at, enosys_at),
        0,
    ));
    p.push(jump(
        BPF_JEQ_K,
        libc::SYS_clone as u32,
        rel(clone_at, check_ld),
        rel(clone_at, allow_at),
    ));
    // clone の第 1 引数（flags）の下位 32 bit（seccomp_data.args[0]、リトルエンディアン）。
    p.push(stmt(BPF_LD_W_ABS, 16));
    p.push(jump(
        BPF_JSET_K,
        CLONE_NEW_MASK,
        rel(check_jset, deny_at),
        rel(check_jset, allow_at),
    ));
    debug_assert_eq!(p.len(), allow_at);
    p.push(stmt(BPF_RET_K, RET_ALLOW));
    p.push(stmt(BPF_RET_K, RET_ERRNO | libc::EPERM as u32));
    p.push(stmt(BPF_RET_K, RET_ERRNO | libc::ENOSYS as u32));
    p
}

fn install_seccomp() -> Result<(), String> {
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        return Err("unsupported architecture".into());
    }
    let mut filter = build_filter();
    let prog = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_mut_ptr(),
    };
    // SAFETY: `prog` は `filter` を指し、呼び出しの間 `filter` は生きている。カーネルが複製する。
    // TSYNC で既にあるスレッド（ライブラリが作ったもの）にも同じフィルターを掛ける。
    let r = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            libc::SECCOMP_FILTER_FLAG_TSYNC,
            &prog as *const libc::sock_fprog,
        )
    };
    if r != 0 {
        return Err(format!("seccomp returned {r} (errno {})", errno()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 自己点検

/// `socket()` が `EPERM` で拒否されるか。
pub fn socket_denied() -> bool {
    // SAFETY: 整数引数だけ。成功したら閉じる。
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd >= 0 {
        // SAFETY: 上で開いた fd。
        unsafe { libc::close(fd) };
        return false;
    }
    errno() == libc::EPERM
}

/// `dir` に新しいファイルを作れないか。
pub fn fs_write_denied(dir: &Path) -> bool {
    let path = dir.join(format!(
        "senca-codec-worker-selftest-{}",
        std::process::id()
    ));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&path);
            false
        }
        Err(e) => e.kind() == std::io::ErrorKind::PermissionDenied,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_shape() {
        let f = build_filter();
        assert!(f.len() < 255);
        // 最後の 3 命令は ALLOW・DENY・ENOSYS。
        assert_eq!(f[f.len() - 3].k, RET_ALLOW);
        assert_eq!(f[f.len() - 5].k, 16);
        // DENY は EPERM を返す。
        assert!(
            f.iter()
                .any(|i| i.code == BPF_RET_K && i.k == RET_ERRNO | libc::EPERM as u32)
        );
    }

    #[test]
    fn cpu_seconds() {
        assert!(cpu_seconds_used() >= 1);
    }
}
