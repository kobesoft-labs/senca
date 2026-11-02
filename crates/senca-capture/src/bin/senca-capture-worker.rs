//! capture worker の実行ファイル。親（ノード）が標準入出力のパイプでつないで起動する
//! （`senca_capture::client`）。機器の SDK・ドライバー（DeckLink・ALSA・V4L2）はこのプロセスだけが触る。

use senca_capture::worker::{self, Mode};

fn main() {
    let code = match worker::parse_args(std::env::args().skip(1)) {
        Ok(Mode::List(o)) => worker::run_list(&o),
        Ok(Mode::Serve(o)) => worker::serve(&o),
        Ok(Mode::Help) => {
            eprintln!("{}", worker::USAGE);
            0
        }
        Err(e) => {
            eprintln!("senca-capture-worker: {e}\n{}", worker::USAGE);
            64
        }
    };
    std::process::exit(code);
}
