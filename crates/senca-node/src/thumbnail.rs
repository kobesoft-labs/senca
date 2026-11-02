//! 録画のサムネイル（R6-9）。復号が要るので codec worker が使えるときだけ作る（M4）。

use crate::app::App;

pub fn generate(app: &App, recording: &str) {
    if app.codec.is_none() {
        return;
    }
    let _ = recording;
}
