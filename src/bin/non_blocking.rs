use std::io::{self, Read, Write};
use std::net::TcpListener;

/// States for places where we might encounter `WouldBlock`.
enum ConnectionState {
    Read {
        request: [u8; 1024],
        read: usize,
    },
    Write {
        response: &'static [u8],
        written: usize,
    },
    Flush,
}
// Our server can now handle running multiple requests concurrently on a
// single thread. Nothing ever blocks. If some operation would have blocked,
// it remembers the current state and moves on to run something else,
// much like the kernel scheduler was doing for us.
fn main() {
    let listener = TcpListener::bind("localhost:3000").unwrap();
    listener.set_nonblocking(true).unwrap();

    let mut connections = Vec::new();

    loop {
        // Try accepting a new connection.
        match listener.accept() {
            Ok((connection, _)) => {
                connection.set_nonblocking(true).unwrap();

                // Start in the Read state with an empty buffer and zero bytes
                // read.
                let state = ConnectionState::Read {
                    request: [0u8; 1024],
                    read: 0,
                };
                connections.push((connection, state));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => panic!("{e}"),
        }

        // Store a separate list of indices to remove after we finish.
        let mut completed = Vec::new();

        // Try to drive each connection forward from its current state.
        'next: for (i, (connection, state)) in
            connections.iter_mut().enumerate()
        {
            if let ConnectionState::Read { request, read } = state {
                loop {
                    // Try reading from the stream.
                    match connection.read(&mut request[*read..]) {
                        Ok(0) => {
                            println!("client disconnected");
                            completed.push(i);
                            continue 'next;
                        }
                        Ok(n) => {
                            // Keep track of how many bytes we've read.
                            *read += n
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            // Not ready yet, move on to the _next_ connection.
                            continue 'next;
                        }
                        Err(e) => panic!("{e}"),
                    }

                    // Did we reach the end of the request?
                    if request.get(*read - 4..*read) == Some(b"\r\n\r\n") {
                        break;
                    }
                }

                // We're done, print the request.
                let request = String::from_utf8_lossy(&request[..*read]);
                println!("{request}");

                // Move into the _Write_ state.
                let response = concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "Content-Length: 12\n",
                    "Connection: close\r\n\r\n",
                    "Hello world!"
                );

                *state = ConnectionState::Write {
                    response: response.as_bytes(),
                    written: 0,
                }
            }
            if let ConnectionState::Write { response, written } = state {
                loop {
                    match connection.write(&response[*written..]) {
                        Ok(0) => {
                            println!("client disconnected");
                            completed.push(i);
                            continue 'next;
                        }
                        Ok(n) => {
                            *written += n;
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            // Not ready yet, move on to the next connection.
                            continue 'next;
                        }
                        Err(e) => panic!("{e}"),
                    }

                    if *written == response.len() {
                        break;
                    }
                }
                // Successfully wrote the response, try flushing next.
                *state = ConnectionState::Flush;
            }
            if let ConnectionState::Flush = state {
                match connection.flush() {
                    Ok(_) => {
                        completed.push(i);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        // Not ready yet, move on to the next connection.
                        continue 'next;
                    }
                    Err(e) => panic!("{e}"),
                }
            }
        }

        // Iterate in reverse order to preserve indices.
        for i in completed.into_iter().rev() {
            connections.remove(i);
        }
    }
}
