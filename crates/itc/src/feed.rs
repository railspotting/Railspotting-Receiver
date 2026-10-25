//! A feed of decoded ITC messages over TCP for monitors (RailSpotting-ITC-Monitor and others): every
//! client that connects gets each message from then on, one JSON line each. Nothing is sent
//! unasked, and a client that stops reading is dropped rather than holding up decoding.
//!
//! Serve it on 127.0.0.1 unless the monitor runs on another machine you control: decoded messages
//! describe railroad operations.

use std::io::{self, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long a write to a slow client may block before it is dropped.
const WRITE_TIMEOUT: Duration = Duration::from_secs(1);

pub struct Feed {
    clients: Arc<Mutex<Vec<TcpStream>>>,
    pub addr: SocketAddr,
}

impl Feed {
    /// Listen on `addr` and take clients on a thread of its own.
    pub fn serve(addr: &str) -> io::Result<Feed> {
        let l = TcpListener::bind(addr)?;
        let addr = l.local_addr()?;
        let clients: Arc<Mutex<Vec<TcpStream>>> = Arc::default();
        let c = clients.clone();
        std::thread::spawn(move || {
            for conn in l.incoming() {
                let Ok(s) = conn else { continue };
                if s.set_write_timeout(Some(WRITE_TIMEOUT)).is_err() {
                    continue;
                }
                let _ = s.set_nodelay(true);
                log::info!("ITC feed: client {}", s.peer_addr().map_or("?".into(), |a| a.to_string()));
                c.lock().unwrap_or_else(|p| p.into_inner()).push(s);
            }
        });
        Ok(Feed { clients, addr })
    }

    /// One message to every client; those that cannot take it are dropped.
    pub fn send(&self, line: &str) {
        let mut cs = self.clients.lock().unwrap_or_else(|p| p.into_inner());
        cs.retain_mut(|s| {
            let ok = s.write_all(line.as_bytes()).and_then(|_| s.write_all(b"\n")).is_ok();
            if !ok {
                log::info!("ITC feed: client {} gone", s.peer_addr().map_or("?".into(), |a| a.to_string()));
            }
            ok
        });
    }

    #[cfg(test)]
    fn clients(&self) -> usize {
        self.clients.lock().unwrap_or_else(|p| p.into_inner()).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};

    #[test]
    fn clients_get_each_line() {
        let f = Feed::serve("127.0.0.1:0").unwrap();
        let c = TcpStream::connect(f.addr).unwrap();
        // the accept runs on its own thread
        for _ in 0..200 {
            if f.clients() == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(f.clients(), 1);
        f.send("{\"type\":\"TIME\"}");
        f.send("{\"type\":\"CNTL\"}");
        let mut lines = BufReader::new(c).lines();
        assert_eq!(lines.next().unwrap().unwrap(), "{\"type\":\"TIME\"}");
        assert_eq!(lines.next().unwrap().unwrap(), "{\"type\":\"CNTL\"}");
        drop(lines);
        // a client that has gone is dropped once a write fails (the first writes after it closes
        // can still succeed, until its reset arrives)
        for _ in 0..200 {
            f.send("x");
            if f.clients() == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(f.clients(), 0);
    }
}
