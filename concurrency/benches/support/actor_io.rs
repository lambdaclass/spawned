//! Identical native echo server for full-actor Tokio/sharded client comparisons.
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::JoinHandle,
    time::Duration,
};
pub struct EchoServer {
    pub addr: SocketAddr,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl EchoServer {
    pub fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let worker = std::thread::spawn(move || {
            let mut peers = Vec::new();
            while !flag.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        // macOS can inherit O_NONBLOCK from the accept socket.
                        socket.set_nonblocking(false).unwrap();
                        socket.set_nodelay(true).unwrap();
                        socket
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .unwrap();
                        peers.push(std::thread::spawn(move || {
                            let mut frame = [0; 64];
                            while socket.read_exact(&mut frame).is_ok() {
                                if socket.write_all(&frame).is_err() {
                                    break;
                                }
                            }
                        }));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1))
                    }
                    Err(e) => panic!("echo accept: {e}"),
                }
            }
            for peer in peers {
                peer.join().unwrap();
            }
        });
        Self {
            addr,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for EchoServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
    }
}
