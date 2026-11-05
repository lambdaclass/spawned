//! I/O mechanism control: persistent TCP echo, not a full actor backend.
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Barrier},
    time::{Duration, Instant},
};
fn args(key: &str, default: &str) -> String {
    std::env::args()
        .find_map(|s| s.strip_prefix(&format!("--{key}=")).map(str::to_owned))
        .unwrap_or_else(|| default.into())
}
fn main() {
    let backend = args("backend", "tokio-io");
    let connections: usize = args("connections", "16").parse().unwrap();
    let messages: usize = args("messages", "2000").parse().unwrap();
    assert!(connections > 0 && connections <= 256 && messages > 0);
    #[cfg(target_os = "linux")]
    if backend == "monoio-uring" {
        // Fail before spawning barrier participants when the VM/seccomp denies io_uring.
        let _ = monoio::RuntimeBuilder::<monoio::IoUringDriver>::new()
            .build()
            .expect("io_uring preflight");
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let mut joins = Vec::new();
        for stream in listener.incoming().take(connections) {
            let mut stream = stream.unwrap();
            stream.set_nodelay(true).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(15)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(15)))
                .unwrap();
            joins.push(std::thread::spawn(move || {
                let mut data = [0u8; 64];
                loop {
                    match stream.read_exact(&mut data) {
                        Ok(()) => stream.write_all(&data).unwrap(),
                        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                        Err(e) => panic!("echo server: {e}"),
                    }
                }
            }));
        }
        for j in joins {
            j.join().unwrap();
        }
    });
    let ready = Arc::new(Barrier::new(connections + 1));
    let done = Arc::new(Barrier::new(connections + 1));
    let mut clients = Vec::new();
    for id in 0..connections {
        let (ready, done, backend) = (ready.clone(), done.clone(), backend.clone());
        clients.push(std::thread::spawn(move || {
            let run_std = || {
                let mut stream = TcpStream::connect(address).unwrap();
                stream.set_nodelay(true).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(15)))
                    .unwrap();
                let mut buf = [0u8; 64];
                // All backends warm each connection before the start barrier.
                stream.write_all(&buf).unwrap();
                stream.read_exact(&mut buf).unwrap();
                ready.wait();
                ready.wait();
                let mut latencies = Vec::with_capacity(messages);
                for seq in 0..messages {
                    buf[..8].copy_from_slice(&((id * messages + seq) as u64).to_le_bytes());
                    let expected = buf;
                    let start = Instant::now();
                    stream.write_all(&buf).unwrap();
                    stream.read_exact(&mut buf).unwrap();
                    latencies.push(start.elapsed().as_nanos() as u64);
                    assert_eq!(buf, expected);
                }
                done.wait();
                latencies
            };
            let run_tokio = || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
                        stream.set_nodelay(true).unwrap();
                        let mut buf = [0u8; 64];
                        stream.write_all(&buf).await.unwrap();
                        stream.read_exact(&mut buf).await.unwrap();
                        ready.wait();
                        ready.wait();
                        let mut latencies = Vec::with_capacity(messages);
                        for seq in 0..messages {
                            buf[..8].copy_from_slice(&((id * messages + seq) as u64).to_le_bytes());
                            let expected = buf;
                            let start = Instant::now();
                            stream.write_all(&buf).await.unwrap();
                            stream.read_exact(&mut buf).await.unwrap();
                            latencies.push(start.elapsed().as_nanos() as u64);
                            assert_eq!(buf, expected);
                        }
                        done.wait();
                        latencies
                    })
            };
            let run_monoio = async {
                use monoio::io::{AsyncReadRentExt, AsyncWriteRentExt};
                let mut stream = monoio::net::TcpStream::connect(address).await.unwrap();
                stream.set_nodelay(true).unwrap();
                let buf = vec![0u8; 64];
                let (res, buf) = stream.write_all(buf).await;
                res.unwrap();
                let (res, mut buf) = stream.read_exact(buf).await;
                res.unwrap();
                ready.wait();
                ready.wait();
                let mut latencies = Vec::with_capacity(messages);
                for seq in 0..messages {
                    let request = (id * messages + seq) as u64;
                    buf[..8].copy_from_slice(&request.to_le_bytes());
                    let start = Instant::now();
                    let (res, next) = stream.write_all(buf).await;
                    res.unwrap();
                    let (res, next) = stream.read_exact(next).await;
                    res.unwrap();
                    buf = next;
                    latencies.push(start.elapsed().as_nanos() as u64);
                    assert_eq!(u64::from_le_bytes(buf[..8].try_into().unwrap()), request);
                    assert!(buf[8..].iter().all(|byte| *byte == 0));
                }
                done.wait();
                latencies
            };
            match backend.as_str() {
                "std-io" => run_std(),
                "tokio-io" => run_tokio(),
                "monoio-legacy" => monoio::RuntimeBuilder::<monoio::LegacyDriver>::new()
                    .build()
                    .unwrap()
                    .block_on(run_monoio),
                #[cfg(target_os = "linux")]
                "monoio-uring" => monoio::RuntimeBuilder::<monoio::IoUringDriver>::new()
                    .build()
                    .unwrap()
                    .block_on(run_monoio),
                _ => panic!("backend unavailable on this OS"),
            }
        }));
    }
    ready.wait();
    let start = Instant::now();
    ready.wait();
    done.wait();
    let elapsed = start.elapsed();
    let mut latencies = Vec::new();
    for client in clients {
        latencies.extend(client.join().unwrap());
    }
    server.join().unwrap();
    latencies.sort_unstable();
    let p = |percent: usize| latencies[(latencies.len() * percent).div_ceil(100) - 1];
    println!(
        "backend,connections,messages,payload_bytes,elapsed_s,completed_per_s,p50_ns,p95_ns,p99_ns"
    );
    println!(
        "{backend},{connections},{},{},{:.9},{:.3},{},{},{}",
        connections * messages,
        64,
        elapsed.as_secs_f64(),
        (connections * messages) as f64 / elapsed.as_secs_f64(),
        p(50),
        p(95),
        p(99)
    );
}
