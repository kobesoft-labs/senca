//! std の同期 I/O で `TlsSession` を包む簡単な adapter（例と相互接続試験用）。
//! node 側の async の糊はこれと同じ形で書く。

use std::io::{self, Read, Write};

use senca_tls::TlsSession;

pub struct TlsStream<C: TlsSession, S: Read + Write> {
    pub conn: C,
    pub sock: S,
    rbuf: Vec<u8>,
    rpos: usize,
    inbuf: Vec<u8>,
    eof: bool,
}

fn to_io(e: senca_tls::Error) -> io::Error {
    io::Error::other(e.to_string())
}

impl<C: TlsSession, S: Read + Write> TlsStream<C, S> {
    pub fn new(conn: C, sock: S) -> Self {
        Self {
            conn,
            sock,
            rbuf: Vec::new(),
            rpos: 0,
            inbuf: Vec::new(),
            eof: false,
        }
    }

    pub fn flush_tls(&mut self) -> io::Result<()> {
        while self.conn.wants_write() {
            let d = self.conn.take_tls();
            self.sock.write_all(&d)?;
        }
        self.sock.flush()
    }

    /// ソケットから読み、TLS に渡す。0 なら EOF。
    fn read_more(&mut self) -> io::Result<usize> {
        if self.inbuf.is_empty() {
            let mut buf = [0u8; 16 * 1024];
            let n = self.sock.read(&mut buf)?;
            if n == 0 {
                self.eof = true;
                return Ok(0);
            }
            self.inbuf.extend_from_slice(&buf[..n]);
        }
        let r = self.conn.read_tls(&self.inbuf);
        // エラーでも alert を送ってから返す。
        let flushed = self.flush_tls();
        let used = r.map_err(to_io)?;
        flushed?;
        self.inbuf.drain(..used);
        while let Some(p) = self.conn.take_plaintext() {
            self.rbuf.extend_from_slice(&p);
        }
        Ok(used.max(1))
    }

    pub fn complete_handshake(&mut self) -> io::Result<()> {
        self.flush_tls()?;
        while self.conn.is_handshaking() {
            if self.read_more()? == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "ハンドシェイク中に EOF"));
            }
        }
        self.flush_tls()
    }

    pub fn close(&mut self) -> io::Result<()> {
        self.conn.send_close_notify();
        self.flush_tls()
    }
}

impl<C: TlsSession, S: Read + Write> Read for TlsStream<C, S> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.rpos < self.rbuf.len() {
                let n = out.len().min(self.rbuf.len() - self.rpos);
                out[..n].copy_from_slice(&self.rbuf[self.rpos..self.rpos + n]);
                self.rpos += n;
                if self.rpos == self.rbuf.len() {
                    self.rbuf.clear();
                    self.rpos = 0;
                }
                return Ok(n);
            }
            if self.conn.peer_closed() || self.eof {
                return Ok(0);
            }
            self.read_more()?;
        }
    }
}

impl<C: TlsSession, S: Read + Write> Write for TlsStream<C, S> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let n = self.conn.write_plaintext(data).map_err(to_io)?;
        self.flush_tls()?;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_tls()
    }
}
