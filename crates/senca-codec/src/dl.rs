//! 共有ライブラリの実行時読み込み（dlopen）。
//!
//! # 境界
//!
//! - 読み込むのは、各プロバイダーが固定で持つ soname の一覧（`libdav1d.so.7` など）だけ。名前に `/` を含むものは拒否する。
//!   探す場所は運用者の設定（[`LibrarySearch`] のディレクトリ、環境変数 `SENCA_CODEC_LIB_DIR`）と、
//!   システムの既定（dlopen の検索順）。API の利用者が任意のパスを読み込ませることはできない（仕様 9.4）。
//! - 読み込んだライブラリは [`Library`] を `Arc` で共有し、そこから取り出した関数ポインターを使う間は必ず保持する。
//!
//! このモジュールは `unsafe` を使う（lib.rs で `#[allow(unsafe_code)]`）。

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// ライブラリを探す場所。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LibrarySearch {
    /// 先に探すディレクトリ（順に）。見つからなければシステムの既定で探す。
    pub dirs: Vec<PathBuf>,
    /// `true` ならシステムの既定では探さない（試験で「ライブラリが無い」を再現する）。
    pub dirs_only: bool,
}

/// 環境変数 `SENCA_CODEC_LIB_DIR`（`:` 区切り）。
pub const LIB_DIR_ENV: &str = "SENCA_CODEC_LIB_DIR";

impl LibrarySearch {
    pub fn new(dirs: Vec<PathBuf>) -> Self {
        LibrarySearch {
            dirs,
            dirs_only: false,
        }
    }

    /// `SENCA_CODEC_LIB_DIR` から作る。
    pub fn from_env() -> Self {
        let dirs = std::env::var_os(LIB_DIR_ENV)
            .map(|v| {
                std::env::split_paths(&v)
                    .filter(|p| !p.as_os_str().is_empty())
                    .collect()
            })
            .unwrap_or_default();
        LibrarySearch {
            dirs,
            dirs_only: false,
        }
    }

    /// `names` を順に試し、最初に読み込めたものを返す。失敗の理由はまとめて返す（装置が無いのは正常なので落ちない）。
    pub fn open(&self, names: &[&str]) -> Result<Arc<Library>, String> {
        let mut errors = Vec::new();
        for name in names {
            if name.contains('/') || name.is_empty() {
                errors.push(format!("{name}: invalid library name"));
                continue;
            }
            for dir in &self.dirs {
                let path = dir.join(name);
                if !path.exists() {
                    continue;
                }
                match Library::open_path(&path) {
                    Ok(l) => return Ok(Arc::new(l)),
                    Err(e) => errors.push(e),
                }
            }
            if !self.dirs_only {
                match Library::open_path(Path::new(name)) {
                    Ok(l) => return Ok(Arc::new(l)),
                    Err(e) => errors.push(e),
                }
            }
        }
        if errors.is_empty() {
            errors.push(format!("{} not found", names.join(", ")));
        }
        Err(errors.join("; "))
    }
}

/// 読み込んだ共有ライブラリ。
pub struct Library {
    inner: libloading::Library,
    /// 読み込んだ名前（またはパス）。
    pub name: String,
}

impl std::fmt::Debug for Library {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Library").field("name", &self.name).finish()
    }
}

impl Library {
    fn open_path(path: &Path) -> Result<Library, String> {
        let os: OsString = path.as_os_str().to_owned();
        // SAFETY: dlopen はライブラリの初期化関数（ELF の .init_array）を実行する。読み込むのは各プロバイダーが
        // 固定で持つ既知のコーデック・ドライバーのライブラリだけで（名前は定数、ディレクトリは運用者の設定）、
        // その初期化関数が健全であることはライブラリの提供者に依存する（これが FFI の信頼境界）。
        // 万一ライブラリが異常終了しても、codec worker のプロセスの中で閉じる（仕様 3.1・AC-17）。
        let inner = unsafe { libloading::Library::new(&os) }
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let name = String::from_utf8_lossy(&os.into_vec()).into_owned();
        Ok(Library { inner, name })
    }

    /// 記号を取り出し、関数ポインター（または変数のポインター）として返す。
    ///
    /// # Safety
    ///
    /// `T` は C の宣言（ヘッダー）と一致する関数ポインター型（`unsafe extern "C" fn(...)`）でなければならない。
    /// 返した値は、この `Library` が生きている間だけ使ってよい（呼び出し側は `Arc<Library>` を保持する）。
    pub unsafe fn sym<T: Copy>(&self, name: &str) -> Result<T, String> {
        // SAFETY: 型の一致は呼び出し側が保証する（関数の契約）。`Symbol` から値を複製して取り出すが、
        // ライブラリの寿命は呼び出し側が `Arc<Library>` で保つ。
        unsafe {
            self.inner
                .get::<T>(name)
                .map(|s| *s)
                .map_err(|e| format!("{}: symbol {name}: {e}", self.name))
        }
    }

    /// 記号があるか（版の判定用）。
    pub fn has(&self, name: &str) -> bool {
        // SAFETY: `*const c_void` として取り出すだけで、呼ばない・読まない。
        unsafe { self.inner.get::<*const std::ffi::c_void>(name).is_ok() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_library_is_error() {
        let s = LibrarySearch {
            dirs: vec!["/nonexistent".into()],
            dirs_only: true,
        };
        let e = s.open(&["libdoesnotexist.so.1"]).unwrap_err();
        assert!(e.contains("not found"), "{e}");
        let s = LibrarySearch::default();
        assert!(s.open(&["libdoesnotexist-senca.so.1"]).is_err());
        assert!(s.open(&["../evil.so"]).unwrap_err().contains("invalid"));
    }

    #[test]
    fn opens_libc() {
        let s = LibrarySearch::default();
        let l = s.open(&["libc.so.6"]).unwrap();
        assert!(l.has("strlen"));
        assert!(!l.has("senca_no_such_symbol"));
        // SAFETY: strlen の宣言どおりの型。
        let f: unsafe extern "C" fn(*const std::ffi::c_char) -> usize =
            unsafe { l.sym("strlen") }.unwrap();
        // SAFETY: NUL 終端の文字列を渡す。
        assert_eq!(unsafe { f(c"abc".as_ptr()) }, 3);
    }
}
