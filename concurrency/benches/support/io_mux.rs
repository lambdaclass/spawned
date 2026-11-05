//! One client reactor multiplexing all sockets; common server across implementations.
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::mpsc,
    time::{Duration, Instant},
};
pub fn run(backend: &str, connections: usize, messages: usize) {
    #[cfg(target_os = "linux")]
    if backend == "monoio-uring-mux" {
        let _ = monoio::RuntimeBuilder::<monoio::IoUringDriver>::new()
            .build()
            .expect("io_uring preflight");
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let mut joins = Vec::new();
        for socket in listener.incoming().take(connections) {
            let mut socket = socket.unwrap();
            socket.set_nodelay(true).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(15)))
                .unwrap();
            joins.push(std::thread::spawn(move || {
                let mut buf = [0u8; 64];
                loop {
                    match socket.read_exact(&mut buf) {
                        Ok(()) => socket.write_all(&buf).unwrap(),
                        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                        Err(e) => panic!("server: {e}"),
                    }
                }
            }));
        }
        for j in joins {
            j.join().unwrap();
        }
    });
    let (ready, rx) = mpsc::channel();
    let mut starts = Vec::new();
    let mut gates = Vec::new();
    for _ in 0..connections {
        let (tx, rx) = futures::channel::oneshot::channel();
        starts.push(tx);
        gates.push(rx);
    }
    let (finished, finish_rx) = mpsc::channel();
    let mode = backend.to_owned();
    let client = std::thread::spawn(move || {
        if mode == "tokio-mux" {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let values = futures::future::join_all(gates.into_iter().enumerate().map(
                        |(id, gate)| {
                            let ready = ready.clone();
                            async move {
                                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                                let mut socket =
                                    tokio::net::TcpStream::connect(addr).await.unwrap();
                                socket.set_nodelay(true).unwrap();
                                let mut buf = [0u8; 64];
                                socket.write_all(&buf).await.unwrap();
                                socket.read_exact(&mut buf).await.unwrap();
                                let mut values = Vec::with_capacity(messages);
                                ready.send(()).unwrap();
                                gate.await.unwrap();
                                for seq in 0..messages {
                                    let request = (id * messages + seq) as u64;
                                    buf[..8].copy_from_slice(&request.to_le_bytes());
                                    let expected = buf;
                                    let start = Instant::now();
                                    socket.write_all(&buf).await.unwrap();
                                    socket.read_exact(&mut buf).await.unwrap();
                                    values.push(start.elapsed().as_nanos() as u64);
                                    assert_eq!(buf, expected);
                                }
                                values
                            }
                        },
                    ))
                    .await;
                    finished.send(Instant::now()).unwrap();
                    values
                })
        } else {
            let body = async {
                let values =
                    futures::future::join_all(gates.into_iter().enumerate().map(|(id, gate)| {
                        let ready = ready.clone();
                        async move {
                            use monoio::io::{AsyncReadRentExt, AsyncWriteRentExt};
                            let mut socket = monoio::net::TcpStream::connect(addr).await.unwrap();
                            socket.set_nodelay(true).unwrap();
                            let (res, buf) = socket.write_all(vec![0u8; 64]).await;
                            res.unwrap();
                            let (res, mut buf) = socket.read_exact(buf).await;
                            res.unwrap();
                            let mut values = Vec::with_capacity(messages);
                            ready.send(()).unwrap();
                            gate.await.unwrap();
                            for seq in 0..messages {
                                let request = (id * messages + seq) as u64;
                                buf[..8].copy_from_slice(&request.to_le_bytes());
                                let start = Instant::now();
                                let (res, next) = socket.write_all(buf).await;
                                res.unwrap();
                                let (res, next) = socket.read_exact(next).await;
                                res.unwrap();
                                buf = next;
                                values.push(start.elapsed().as_nanos() as u64);
                                assert_eq!(
                                    u64::from_le_bytes(buf[..8].try_into().unwrap()),
                                    request
                                );
                                assert!(buf[8..].iter().all(|b| *b == 0));
                            }
                            values
                        }
                    }))
                    .await;
                finished.send(Instant::now()).unwrap();
                values
            };
            match mode.as_str() {
                "monoio-legacy-mux" => monoio::RuntimeBuilder::<monoio::LegacyDriver>::new()
                    .build()
                    .unwrap()
                    .block_on(body),
                #[cfg(target_os = "linux")]
                "monoio-uring-mux" => monoio::RuntimeBuilder::<monoio::IoUringDriver>::new()
                    .build()
                    .unwrap()
                    .block_on(body),
                _ => panic!("unsupported mux backend"),
            }
        }
    });
    for _ in 0..connections {
        rx.recv_timeout(Duration::from_secs(15))
            .expect("socket initialization");
    }
    let start = Instant::now();
    for tx in starts {
        tx.send(()).unwrap();
    }
    let finish = finish_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("I/O completion");
    let elapsed = finish.duration_since(start);
    let batches = client.join().unwrap();
    server.join().unwrap();
    let mut values: Vec<_> = batches.into_iter().flatten().collect();
    assert_eq!(values.len(), connections * messages);
    values.sort_unstable();
    let p = |n: usize| values[(values.len() * n).div_ceil(100) - 1];
    println!(
        "backend,connections,messages,payload_bytes,elapsed_s,completed_per_s,p50_ns,p95_ns,p99_ns"
    );
    println!(
        "{backend},{connections},{},64,{:.9},{:.3},{},{},{}",
        values.len(),
        elapsed.as_secs_f64(),
        values.len() as f64 / elapsed.as_secs_f64(),
        p(50),
        p(95),
        p(99)
    );
}
