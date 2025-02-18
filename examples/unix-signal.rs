use std::io;

#[cfg(unix)]
fn main() -> io::Result<()> {
    use std::{
        os::unix::net::UnixStream,
        time::{Duration, Instant},
    };

    use futures_lite::AsyncReadExt;
    use local_runtime::{block_on, io::Async, time::timeout};

    block_on(async {
        let (tx, rx) = UnixStream::pair()?;
        let mut rx = Async::new(rx)?;
        signal_hook::low_level::pipe::register(signal_hook::consts::SIGINT, tx)?;

        println!("Waiting for ctrl-C");
        let start = Instant::now();
        // Wait for 10 second timeout
        match timeout(rx.read_exact(&mut [0]), Duration::from_secs(10)).await {
            Ok(res) => {
                res?;
                println!("Done after {} ms", start.elapsed().as_millis());
            }
            Err(_) => println!("Timed out after {} ms", start.elapsed().as_millis()),
        };

        Ok(())
    })
}

#[cfg(not(unix))]
fn main() {
    println!("This only works on Unix");
}
