//! codec worker の実行ファイル。親（`WorkerPool`）が標準入出力のパイプでつないで起動する。

use senca_codec_worker::worker::{self, Mode};

fn main() {
    let code = match worker::parse_args(std::env::args().skip(1)) {
        Ok(Mode::Serve(o)) => worker::serve(&o),
        Ok(Mode::SelfTestSandbox(o)) => worker::run_self_test(&o),
        Ok(Mode::Help) => {
            eprintln!("{}", worker::USAGE);
            0
        }
        Err(e) => {
            eprintln!("senca-codec-worker: {e}\n{}", worker::USAGE);
            64
        }
    };
    std::process::exit(code);
}
